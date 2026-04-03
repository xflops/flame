---
Issue: TBD
Author: Flame Team
Date: 2026-03-31
---

# Design Document: mTLS Authentication and Authorization with Workspaces

## 1. Motivation

**Background:**

Flame currently supports TLS/mTLS for transport-layer security (RFE234), encrypting all communication between components (clients, session manager, executor manager). However, TLS alone does not provide **application-level authentication or authorization**—any client with network access and valid TLS certificates can access any session or application in the cluster.

Current security gaps:

1. **No Identity**: Flame cannot identify WHO is making requests beyond TLS certificate validation
2. **No Multi-tenancy**: All sessions, applications, and executors exist in a flat namespace—any client can access any resource
3. **No Authorization**: No way to restrict which clients can access which resources
4. **No Cache Isolation**: Flame instances accessing the Object Cache cannot be distinguished from external clients

**Target:**

Implement workspace-based multi-tenancy with mTLS-based authentication to ensure:

1. **Workspaces**: Logical isolation boundaries for all Flame objects (applications, sessions, executors)
2. **Identity via mTLS**: Client certificates provide cryptographic identity (Subject/CN)
3. **Authorization**: Map certificate subjects to permitted workspaces
4. **Internal Access**: Flame components (executor manager, session manager) can access cache with elevated privileges
5. **Backward Compatibility**: Default `system` workspace maintains current behavior for single-tenant deployments

## 2. Function Specification

### 2.1 Workspace Concept

A **Workspace** is a logical isolation boundary that groups related resources. All Flame objects belong to exactly one workspace.

```
┌──────────────────────────────────────────────────────────────────┐
│                        Flame Cluster                             │
│                                                                  │
│  ┌───────────────────────┐    ┌───────────────────────┐          │
│  │   Workspace: team-a   │    │   Workspace: team-b   │          │
│  │                       │    │                       │          │
│  │  ┌─────────────────┐  │    │  ┌─────────────────┐  │          │
│  │  │ Applications:   │  │    │  │ Applications:   │  │          │
│  │  │  - ml-training  │  │    │  │  - data-pipeline│  │          │
│  │  │  - inference    │  │    │  │  - etl-job      │  │          │
│  │  └─────────────────┘  │    │  └─────────────────┘  │          │
│  │                       │    │                       │          │
│  │  ┌─────────────────┐  │    │  ┌─────────────────┐  │          │
│  │  │ Sessions:       │  │    │  │ Sessions:       │  │          │
│  │  │  - train-001    │  │    │  │  - pipeline-001 │  │          │
│  │  │  - infer-002    │  │    │  │  - etl-002      │  │          │
│  │  └─────────────────┘  │    │  └─────────────────┘  │          │
│  │                       │    │                       │          │
│  │  ┌─────────────────┐  │    │  ┌─────────────────┐  │          │
│  │  │ Cache Objects:  │  │    │  │ Cache Objects:  │  │          │
│  │  │  - model-v1     │  │    │  │  - dataset-001  │  │          │
│  │  │  - checkpoint   │  │    │  │  - results      │  │          │
│  │  └─────────────────┘  │    │  └─────────────────┘  │          │
│  └───────────────────────┘    └───────────────────────┘          │
│                                                                  │
│  ┌────────────────────────────────────────────────────────────┐  │
│  │          Workspace: default (pre-defined, for clients)     │  │
│  │  - Applications, Sessions, Tasks when workspace omitted    │  │
│  └────────────────────────────────────────────────────────────┘  │
│                                                                  │
│  ┌───────────────────────────────────────────────────────────┐   │
│  │          Workspace: system (pre-defined, internal only)   │   │
│  │  - Nodes, Executors (managed by Flame, not users)         │   │
│  └───────────────────────────────────────────────────────────┘   │
└──────────────────────────────────────────────────────────────────┘
```

**Workspace Rules:**

| Property     | Rule                                                                                    |
| ------------ | --------------------------------------------------------------------------------------- |
| Pre-defined  | `default` for client resources, `system` for internal components                        |
| Naming       | Alphanumeric with hyphens, max 63 chars (DNS label format)                              |
| Immutability | Workspace cannot be changed after resource creation                                     |
| Inheritance  | Sessions inherit workspace from the request context, not from application               |
| Uniqueness   | Resource names are unique within a workspace (e.g., `team-a/my-app` vs `team-b/my-app`) |

**Pre-defined Workspaces (not configurable):**

| Workspace | Purpose                                                     | Resources                                    |
| --------- | ----------------------------------------------------------- | -------------------------------------------- |
| `default` | Default workspace for client resources when not specified   | Applications, Sessions, Tasks, Cache Objects |
| `system`  | Internal Flame components (not accessible to regular users) | Nodes, Executors                             |

```
┌─────────────────────────────────────────────────────────────────┐
│                        Flame Cluster                            │
│                                                                 │
│  ┌───────────────────────┐    ┌───────────────────────┐         │
│  │   Workspace: team-a   │    │   Workspace: team-b   │         │
│  │   (user-defined)      │    │   (user-defined)      │         │
│  │                       │    │                       │         │
│  │  Applications         │    │  Applications         │         │
│  │  Sessions             │    │  Sessions             │         │
│  │  Tasks                │    │  Tasks                │         │
│  │  Cache Objects        │    │  Cache Objects        │         │
│  └───────────────────────┘    └───────────────────────┘         │
│                                                                 │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │              Workspace: default (pre-defined)             │  │
│  │  Default workspace for client resources when unspecified  │  │
│  │  - Applications, Sessions, Tasks, Cache Objects           │  │
│  └───────────────────────────────────────────────────────────┘  │
│                                                                 │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │              Workspace: system (pre-defined)              │  │
│  │  Internal Flame components only (not user-accessible)     │  │
│  │  - Nodes                                                  │  │
│  │  - Executors                                              │  │
│  └───────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
```

### 2.2 Subject Model (Users and Roles)

A **Subject** represents an authenticated identity—either a **user** or a **role/service account**. Subjects are identified by the Common Name (CN) in their client certificate.

```
┌─────────────────────────────────────────────────────────────────────────┐
│                           Subject Model                                  │
│                                                                         │
│  Subject = User or Role identified by certificate CN                    │
│                                                                         │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │  Subject: "alice" (User)                                         │   │
│  │  Certificate: CN=alice,O=MyOrg                                   │   │
│  │  Permitted Workspaces: [team-a, team-b, shared]                  │   │
│  └─────────────────────────────────────────────────────────────────┘   │
│                                                                         │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │  Subject: "bob" (User)                                           │   │
│  │  Certificate: CN=bob,O=MyOrg                                     │   │
│  │  Permitted Workspaces: [team-b]                                  │   │
│  └─────────────────────────────────────────────────────────────────┘   │
│                                                                         │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │  Subject: "ci-pipeline" (Role/Service Account)                   │   │
│  │  Certificate: CN=ci-pipeline,O=MyOrg                             │   │
│  │  Permitted Workspaces: [team-a, team-b, staging]                 │   │
│  └─────────────────────────────────────────────────────────────────┘   │
│                                                                         │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │  Subject: "admin" (Role)                                         │   │
│  │  Certificate: CN=admin,O=MyOrg                                   │   │
│  │  Permitted Workspaces: ["*"] (all workspaces)                    │   │
│  └─────────────────────────────────────────────────────────────────┘   │
│                                                                         │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │  Subject: "flame-executor" (System User)                        │   │
│  │  Certificate: CN=flame-executor,O=Flame                          │   │
│  │  Permitted Workspaces: ["*"] (root role)                         │   │
│  └─────────────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────────────┘
```

**Key Principle:** Subjects and workspaces are **independent concepts**. A subject (user) is granted access to one or more workspaces through role assignments. All access control is managed via the Admin API.

### 2.3 mTLS Authentication

mTLS extends TLS to require **client certificates**, enabling cryptographic identification of subjects (users/roles).

```
┌──────────────┐                              ┌──────────────────┐
│    Client    │                              │  Session Manager │
│   (alice)    │  1. Client sends certificate │                  │
│              │ ──────────────────────────► │                  │
│  alice.crt   │                              │                  │
│  (CN=alice)  │  2. Server verifies client   │                  │
│              │     cert against CA          │     ca.crt       │
│              │                              │                  │
│              │  3. Server sends certificate │                  │
│              │ ◄────────────────────────── │  server.crt      │
│  ca.crt      │                              │                  │
│              │  4. Client verifies server   │                  │
│              │     cert against CA          │                  │
│              │                              │                  │
│              │  5. Encrypted connection     │                  │
│              │ ◄────────────────────────► │                  │
│              │                              │                  │
│              │  6. Server extracts Subject  │                  │
│              │     CN=alice for authz       │                  │
└──────────────┘                              └──────────────────┘
```

**Certificate Subject Extraction:**

The subject's identity is extracted from the certificate's Common Name (CN):

```
Certificate Subject: CN=alice,O=MyOrg,OU=Engineering
                        ↓
Extracted Identity: "alice" (Common Name = Subject)
```

### 2.4 Authorization Model (RBAC)

Flame uses Role-Based Access Control (RBAC) combined with mTLS:

- **mTLS** provides **authentication** (WHO you are) via certificate CN
- **RBAC** provides **authorization** (WHAT you can do) via roles and permissions

```
┌─────────────────────────────────────────────────────────────────────────┐
│                         RBAC Model                                       │
└─────────────────────────────────────────────────────────────────────────┘

┌─────────────┐      ┌─────────────┐      ┌─────────────┐      ┌──────────┐
│   Subject   │─────►│    Role     │─────►│ Permission  │─────►│ Resource │
│  (cert CN)  │ has  │             │ has  │             │  on  │          │
└─────────────┘      └─────────────┘      └─────────────┘      └──────────┘

Examples:
  alice ──► developer ──► [application:*, session:*] ──► workspace:team-a
  bob ──► viewer ──► [*:read] ──► workspace:team-b
  ci-bot ──► deployer ──► [application:*, session:create] ──► workspace:staging
```

**Core Concepts:**

| Concept        | Description                     | Example                              |
| -------------- | ------------------------------- | ------------------------------------ |
| **Subject**    | Identity from certificate CN    | `alice`, `bob`, `ci-pipeline`        |
| **Role**       | Named collection of permissions | `admin`, `developer`, `viewer`       |
| **Permission** | Action on resource type         | `session:create`, `application:read` |
| **Workspace**  | Scope for permissions           | `team-a`, `production`               |

**Permission Types:**

| Resource      | Permissions                               | Description                                       |
| ------------- | ----------------------------------------- | ------------------------------------------------- |
| `application` | `create`, `read`, `update`, `delete`, `*` | Manage applications                               |
| `session`     | `create`, `read`, `update`, `delete`, `*` | Manage sessions (includes tasks and cache access) |
| `workspace`   | `create`, `read`, `update`, `delete`, `*` | Workspace-level permissions                       |

**RBAC Management via Admin API:**

RBAC (users, roles, bindings) is managed dynamically via the Admin service (`flmadm`), not static configuration files. This allows:
- Dynamic user/role creation without server restart
- Role permission and workspace updates at runtime
- Centralized management via CLI or programmatic API

```bash
# Example: Set up RBAC via flmadm

# Create roles with permissions AND workspaces
flmadm create --role developer \
  --description "Developer role" \
  --permission "application:*" \
  --permission "session:*" \
  --workspace team-a \
  --workspace team-b

flmadm create --role data-scientist \
  --description "Data scientist role" \
  --permission "session:*" \
  --permission "application:read" \
  --workspace experiments \
  --workspace datasets

# Create users and assign roles
flmadm create --user alice --display-name "Alice Smith" --cert-dir .
flmadm update --user alice --assign-role developer,data-scientist

# The effective permissions for alice:
# - developer permissions in [team-a, team-b]
# - data-scientist permissions in [experiments, datasets]
```

**Authorization Flow:**

```
┌─────────────────────────────────────────────────────────────────────────┐
│                      RBAC Authorization Flow                            │
└─────────────────────────────────────────────────────────────────────────┘

1. User "alice" connects with certificate (CN=alice)
                     │
                     ▼
2. TLS layer validates certificate chain
                     │
                     ▼
3. Server extracts subject from certificate CN
   Subject: "alice"
                     │
                     ▼
4. Alice requests: CreateSession(workspace="team-a", ...)
   Required permission: "session:create" on "team-a"
                     │
                     ▼
5. Lookup user "alice" from database, get assigned roles:
   User alice has roles: [developer, data-scientist]
                     │
                     ▼
6. For each role, check if workspace is in role's workspaces:
   developer role:
     workspaces: [team-a, team-b]  ← team-a matches ✓
     permissions: [application:*, session:*]
   
   "session:*" includes "session:create" ✓
                     │
                     ▼
7. Authorization result: ALLOWED
```

**Permission Checking Logic:**

```rust
// common/src/rbac.rs

pub async fn check_permission(
    subject: &str,
    workspace: &str,
    resource: &str,
    action: &str,
    engine: &dyn Engine,  // Uses existing storage.Engine trait
) -> Result<(), AuthzError> {
    let required = format!("{}:{}", resource, action);
    
    // Lookup user and their roles from storage engine
    let user = engine.get_user_by_cn(subject).await?
        .ok_or_else(|| AuthzError::UserNotFound(subject.to_string()))?;
    
    if !user.enabled {
        return Err(AuthzError::UserDisabled(subject.to_string()));
    }
    
    // Get all roles assigned to user
    let roles = engine.get_user_roles(&user.name).await?;
    
    // Check each role for permission in workspace
    for role in roles {
        // Check if workspace matches (including wildcard)
        if !role.workspaces.contains(&workspace.to_string()) 
           && !role.workspaces.contains(&"*".to_string()) {
            continue;
        }
        
        // Check if role has required permission
        if role.has_permission(&required) {
            return Ok(());
        }
    }
    
    Err(AuthzError::PermissionDenied(format!(
        "subject '{}' does not have permission '{}' in workspace '{}'",
        subject, required, workspace
    )))
}

// NOTE: RBAC operations are added to the existing storage::Engine trait
// See session_manager/src/storage/engine/mod.rs for the Engine trait extension

impl Role {
    pub fn has_permission(&self, required: &str) -> bool {
        let (req_resource, req_action) = required.split_once(':').unwrap();
        
        for perm in &self.permissions {
            let (perm_resource, perm_action) = perm.split_once(':').unwrap();
            
            // Check resource match (with wildcard)
            let resource_match = perm_resource == "*" || perm_resource == req_resource;
            
            // Check action match (with wildcard)
            let action_match = perm_action == "*" || perm_action == req_action;
            
            if resource_match && action_match {
                return true;
            }
        }
        
        false
    }
}
```

**Session Certificate Permissions:**

Session-scoped certificates inherit permissions from the parent session's subject:

```
┌─────────────────────────────────────────────────────────────────────────┐
│                 Session Certificate Permissions                         │
└─────────────────────────────────────────────────────────────────────────┘

1. Client "alice" (developer in team-a) creates session
   → Session cert: CN=session:abc123, SAN: workspace=team-a, parent=alice
   → Inherits: developer permissions in team-a

2. Instance using session cert creates child session
   → Child cert: CN=session:def456, SAN: workspace=team-a, parent=abc123
   → Inherits: same permissions as parent session

3. Limitations for session certificates:
   - Cannot escalate permissions beyond parent
   - Workspace is locked (cannot access other workspaces)
   - Some operations restricted (e.g., cannot delete parent session)
```

**Permission Matrix:**

| Operation            | Required Permission  | Session Cert Allowed |
| -------------------- | -------------------- | -------------------- |
| Register application | `application:create` | ✗ (client only)      |
| List applications    | `application:read`   | ✓                    |
| Create session       | `session:create`     | ✓ (child only)       |
| Get session          | `session:read`       | ✓ (own tree)         |
| Close session        | `session:delete`     | ✓ (own + children)   |
| Submit task          | `session:create`     | ✓                    |
| Cancel task          | `session:update`     | ✓ (own tasks)        |
| Put cache object     | `session:create`     | ✓                    |
| Get cache object     | `session:read`       | ✓ (own workspace)    |
| Delete cache object  | `session:delete`     | ✓ (own session)      |

**Multi-Role Example:**

```
User "alice" has multiple role bindings:

Binding 1: developer in [team-a, team-b]
Binding 2: viewer in [shared]
Binding 3: auditor in [production]

Result:
┌─────────────┬──────────────────────────────────────────────────────────┐
│ Workspace   │ Effective Permissions                                    │
├─────────────┼──────────────────────────────────────────────────────────┤
│ team-a      │ application:*, session:* (developer)                     │
│ team-b      │ application:*, session:* (developer)                     │
│ shared      │ *:read (viewer)                                          │
│ production  │ application:read, session:read (auditor)                 │
│ other       │ DENIED (no binding)                                      │
└─────────────┴──────────────────────────────────────────────────────────┘
```

### 2.5 Configuration

**Server Configuration (`flame-cluster.yaml`):**

```yaml
cluster:
  name: flame
  endpoint: "https://flame-session-manager:8080"
  slot: "cpu=1,mem=2g"
  policy: priority
  storage: file://${FLAME_HOME}/data  # or sqlite://${FLAME_HOME}/flame.db
  
  executors:
    shim: host
    limits:
      max_executors: 128
  
  tls:
    # Server identity (same as RFE234)
    cert_file: "${FLAME_HOME}/certs/server.crt"
    key_file: "${FLAME_HOME}/certs/server.key"
    
    # CA for client certificate verification (enables mTLS and authorization)
    ca_file: "${FLAME_HOME}/certs/ca.crt"
    
    # CA key for signing session certificates
    ca_key: "${FLAME_HOME}/certs/ca.key"
    
    # Default validity for session certificates
    cert_validity: 24h

cache:
  endpoint: "grpcs://127.0.0.1:9090"
  network_interface: "eth0"
  storage: "/var/lib/flame/cache"
  
  tls:
    cert_file: "${FLAME_HOME}/certs/server.crt"
    key_file: "${FLAME_HOME}/certs/server.key"
    ca_file: "${FLAME_HOME}/certs/ca.crt"
```

**Note:** Users, roles, and permissions are managed via the Admin API (`flmadm`), not in the configuration file.

**Client Configuration (`~/.flame/flame.yaml`):**

```yaml
current-context: team-a
contexts:
  - name: team-a
    cluster:
      endpoint: "https://flame-session-manager:8080"
      tls:
        # CA certificate for server verification
        ca_file: "/etc/flame/certs/ca.crt"
        # Client certificate and key for mTLS
        cert_file: "/home/user/.flame/certs/team-a.crt"
        key_file: "/home/user/.flame/certs/team-a.key"
    
    # Default workspace for this context
    workspace: team-a
    
    cache:
      endpoint: "grpcs://flame-object-cache:9090"
      tls:
        ca_file: "/etc/flame/certs/ca.crt"
        cert_file: "/home/user/.flame/certs/team-a.crt"
        key_file: "/home/user/.flame/certs/team-a.key"
```

**Configuration Options:**

*TLS Configuration (`cluster.tls.*`):*

| Option      | Type   | Required | Default | Description                                                   |
| ----------- | ------ | -------- | ------- | ------------------------------------------------------------- |
| `cert_file` | string | Yes*     | -       | Path to PEM-encoded server certificate                        |
| `key_file`  | string | Yes*     | -       | Path to PEM-encoded private key                               |
| `ca_file`   | string | Yes**    | -       | Path to PEM-encoded CA for client verification (enables mTLS) |

\* Required when `tls` section is present  
\** When `ca_file` is configured, mTLS and authorization are automatically enabled

*Pre-defined Workspaces (not configurable):*

| Workspace | Purpose                                                                               |
| --------- | ------------------------------------------------------------------------------------- |
| `default` | Used when client doesn't specify a workspace (for applications, sessions, tasks)      |
| `system`  | Reserved for internal components (nodes, executors) - not accessible to regular users |

*Client TLS Configuration (`contexts[].cluster.tls.*`):*

| Option      | Type   | Required | Description                            |
| ----------- | ------ | -------- | -------------------------------------- |
| `ca_file`   | string | No       | CA certificate for server verification |
| `cert_file` | string | Yes*     | Client certificate for mTLS            |
| `key_file`  | string | Yes*     | Client private key for mTLS            |

\* Required when server has mTLS enabled (`tls.ca_file` configured)

### 2.6 API Changes

**Proto Changes (`rpc/protos/types.proto`):**

```protobuf
message Metadata {
  string id = 1;
  string name = 2;
  optional string workspace = 3;  // NEW: Workspace this resource belongs to (optional for User, Role, Workspace)
}

// ============================================================
// RBAC: User, Role, and Workspace Management
// ============================================================

// User represents an authenticated identity (workspace field in metadata is not used)
message User {
  Metadata metadata = 1;             // metadata.workspace is not used for User
  UserSpec spec = 2;
  UserStatus status = 3;
}

message UserSpec {
  string display_name = 1;           // Human-readable name (e.g., "Alice Smith")
  string email = 2;                  // Optional email for notifications
  repeated string role_refs = 3;     // References to Role names (user can have multiple roles)
  string certificate_cn = 4;         // Expected CN in client certificate (must match)
}

message UserStatus {
  int64 creation_time = 1;
  optional int64 last_login_time = 2;
  bool enabled = 3;                  // Can be disabled without deletion
}

message UserList {
  repeated User users = 1;
}

// Role defines a named collection of permissions for workspaces (workspace field in metadata is not used)
message Role {
  Metadata metadata = 1;             // metadata.workspace is not used for Role
  RoleSpec spec = 2;
  RoleStatus status = 3;
}

message RoleSpec {
  string description = 1;            // Human-readable description
  repeated string permissions = 2;    // Permission strings (e.g., "session:create", "application:*")
  repeated string workspaces = 3;     // Workspaces this role grants access to (role can manage multiple)
}

message RoleStatus {
  int64 creation_time = 1;
  int32 user_count = 2;              // Number of users with this role
}

message RoleList {
  repeated Role roles = 1;
}

// Workspace (metadata.workspace is not used - workspace is the resource itself)
message Workspace {
  Metadata metadata = 1;             // metadata.workspace is not used for Workspace
  WorkspaceSpec spec = 2;
  WorkspaceStatus status = 3;
}

message WorkspaceSpec {
  string description = 1;            // Human-readable description
  map<string, string> labels = 2;    // Key-value labels for organization
}

message WorkspaceStatus {
  int64 creation_time = 1;
  int32 session_count = 2;           // Number of active sessions
  int32 application_count = 3;       // Number of registered applications
}

message WorkspaceList {
  repeated Workspace workspaces = 1;
}

// ============================================================
// Session Credential for delegation
// ============================================================

// Credential defines the authentication/authorization context for a session
message Credential {
  string user = 1;                   // User name - must match client's cert CN (or optional if not specified)
  CredentialScope scope = 2;         // Scope of the credential
}

enum CredentialScope {
  CREDENTIAL_SCOPE_UNSPECIFIED = 0;
  CREDENTIAL_SCOPE_USER = 1;         // Delegation cert signed by session manager, user-level access
  CREDENTIAL_SCOPE_SESSION = 2;      // Session-scoped cert, only access resources of this session (e.g., cache)
}

// SessionSpec updated with credential
message SessionSpec {
  string application = 2;
  uint32 slots = 3;
  optional bytes common_data = 4;
  uint32 min_instances = 5;
  optional uint32 max_instances = 6;
  optional Credential credential = 7;  // NEW: Credential for session (determines executor cert scope)
}

// ============================================================
// Standard request/response messages
// ============================================================

// Optional: Explicit workspace in request messages
message CreateSessionRequest {
  string session_id = 1;
  SessionSpec session = 2;
  string workspace = 3;  // NEW: Target workspace (uses default if empty)
}

message RegisterApplicationRequest {
  string name = 1;
  ApplicationSpec application = 2;
  string workspace = 3;  // NEW: Target workspace
}

// List operations can filter by workspace
message ListSessionRequest {
  string workspace = 1;  // NEW: Filter by workspace (empty = all permitted)
}

message ListApplicationRequest {
  string workspace = 1;  // NEW: Filter by workspace
}
```

**Proto Changes (`rpc/protos/frontend.proto`) - Workspace Operations:**

```protobuf
// Add to existing Frontend service in frontend.proto

service Frontend {
  // ... existing RPCs ...
  
  // Workspace operations (user-facing)
  rpc CreateWorkspace(CreateWorkspaceRequest) returns (Workspace) {}
  rpc GetWorkspace(GetWorkspaceRequest) returns (Workspace) {}
  rpc UpdateWorkspace(UpdateWorkspaceRequest) returns (Workspace) {}
  rpc DeleteWorkspace(DeleteWorkspaceRequest) returns (Result) {}
  rpc ListWorkspaces(ListWorkspacesRequest) returns (WorkspaceList) {}
}

// Workspace Management Requests
message CreateWorkspaceRequest {
  string name = 1;
  WorkspaceSpec spec = 2;
}

message GetWorkspaceRequest {
  string name = 1;
}

message UpdateWorkspaceRequest {
  string name = 1;
  WorkspaceSpec spec = 2;
}

message DeleteWorkspaceRequest {
  string name = 1;
  bool force = 2;                    // Force delete even if resources exist
}

message ListWorkspacesRequest {
  // Returns all workspaces the caller has access to
}
```

**New Proto File (`rpc/protos/admin.proto`) - User/Role Management:**

```protobuf
syntax = "proto3";

import "types.proto";

package flame;

option go_package = "github.com/flame-sh/flame/sdk/go/rpc";

/*
 * Admin service for user and role management.
 * Only accessible to subjects with admin role or specific admin permissions.
 */
service Admin {
  // User management
  rpc CreateUser(CreateUserRequest) returns (User) {}
  rpc GetUser(GetUserRequest) returns (User) {}
  rpc UpdateUser(UpdateUserRequest) returns (User) {}
  rpc DeleteUser(DeleteUserRequest) returns (Result) {}
  rpc ListUsers(ListUsersRequest) returns (UserList) {}
  
  // Role management
  rpc CreateRole(CreateRoleRequest) returns (Role) {}
  rpc GetRole(GetRoleRequest) returns (Role) {}
  rpc UpdateRole(UpdateRoleRequest) returns (Role) {}
  rpc DeleteRole(DeleteRoleRequest) returns (Result) {}
  rpc ListRoles(ListRolesRequest) returns (RoleList) {}
}

// ============================================================
// User Management Requests
// ============================================================

message CreateUserRequest {
  string name = 1;                   // Unique username (will be cert CN)
  UserSpec spec = 2;
}

message GetUserRequest {
  string name = 1;
}

message UpdateUserRequest {
  string name = 1;
  UserSpec spec = 2;
  repeated string assign_roles = 3;   // Roles to assign to user
  repeated string revoke_roles = 4;   // Roles to revoke from user
}

message DeleteUserRequest {
  string name = 1;
}

message ListUsersRequest {
  optional string role_filter = 1;   // Filter by role name
}

// ============================================================
// Role Management Requests
// ============================================================

message CreateRoleRequest {
  string name = 1;
  RoleSpec spec = 2;
}

message GetRoleRequest {
  string name = 1;
}

message UpdateRoleRequest {
  string name = 1;
  RoleSpec spec = 2;
}

message DeleteRoleRequest {
  string name = 1;
}

message ListRolesRequest {
  optional string workspace_filter = 1;  // Filter by workspace
}
```

**gRPC Metadata:**

Workspace context can also be passed via gRPC metadata headers:

```
x-flame-workspace: team-a
```

This allows existing API calls to work without modification—the workspace is determined from:
1. Explicit `workspace` field in request message
2. `x-flame-workspace` gRPC metadata header
3. Client's default workspace from config
4. Pre-defined `default` workspace (if none of the above)

**Error Codes:**

| Code | Status            | Description                                   |
| ---- | ----------------- | --------------------------------------------- |
| 7    | PERMISSION_DENIED | Client not authorized for requested workspace |
| 16   | UNAUTHENTICATED   | No valid client certificate provided          |
| 3    | INVALID_ARGUMENT  | Invalid workspace name                        |

### 2.7 CLI Changes

**`flmctl` workspace support:**

```bash
# Set default workspace for commands
flmctl --workspace team-a list -s

# Or use environment variable
export FLAME_WORKSPACE=team-a
flmctl list -s

# Override in config file
# ~/.flame/flame.yaml
# contexts:
#   - name: team-a
#     workspace: team-a

# List sessions across all accessible workspaces
flmctl list -s --all-workspaces

# Create session in specific workspace
flmctl create -a my-app -s 4 --workspace team-a

# Register application in workspace
flmctl register -f app.yaml --workspace team-a

# Create session with credential specification
flmctl create -a my-app -s 4 --workspace team-a \
  --credential-user alice \
  --credential-scope user  # or "session"

# View session details
flmctl view -s <session-id>

# Close session
flmctl close -s <session-id>
```

**`flmadm` user/role/workspace management:**

The `flmadm` CLI provides administrative commands for managing users, roles, and workspaces. Commands follow `verb + --noun` format (e.g., `flmadm create --user alice`). These commands require admin credentials.

```bash
# ============================================================
# User Management
# ============================================================

# Create a new user (generates certificate to output dir)
flmadm create --user alice \
  --display-name "Alice Smith" \
  --email "alice@example.com" \
  --cert-dir .  # Outputs alice.crt and alice.key

# List all users
flmadm list --user

# List users with a specific role
flmadm list --user --role developer

# Get user details
flmadm get --user alice

# Update user (add email, change display name, assign/revoke roles)
flmadm update --user alice --email "alice.smith@example.com"
flmadm update --user alice --assign-role developer,data-scientist
flmadm update --user alice --revoke-role data-scientist

# Disable a user (prevents login without deletion)
flmadm disable --user alice

# Enable a previously disabled user
flmadm enable --user alice

# Delete a user
flmadm delete --user alice

# ============================================================
# Role Management
# ============================================================

# List all roles
flmadm list --role

# Get role details with permissions and workspaces
flmadm get --role developer

# Get role with users assigned to it
flmadm get --role developer --show-users

# Create a role with permissions and workspaces
flmadm create --role data-scientist \
  --description "Data scientist role with ML permissions" \
  --permission "session:*" \
  --permission "application:read" \
  --workspace experiments \
  --workspace datasets \
  --workspace shared

# Update role (add/remove permissions or workspaces)
flmadm update --role data-scientist \
  --add-permission "application:create" \
  --add-workspace production

flmadm update --role data-scientist \
  --remove-permission "application:create" \
  --remove-workspace experiments

# Delete a role
flmadm delete --role data-scientist

# ============================================================
# Workspace Management
# ============================================================

# List all workspaces
flmadm list --workspace

# Create a workspace
flmadm create --workspace team-alpha \
  --description "Team Alpha's workspace" \
  --label team=alpha \
  --label env=development

# Get workspace details (including session/app counts)
flmadm get --workspace team-alpha

# Update workspace
flmadm update --workspace team-alpha \
  --description "Updated description" \
  --label env=staging

# Delete workspace (requires --force if resources exist)
flmadm delete --workspace team-alpha
flmadm delete --workspace team-alpha --force
```

**Server Configuration (`flame-cluster.yaml`):**

Authorization is enabled when `tls.ca_file` is configured (mTLS). Users, roles, and permissions are managed entirely via the Admin API - no RBAC configuration in the config file.

```yaml
cluster:
  name: flame
  endpoint: "https://flame-session-manager:8080"
  
  tls:
    cert_file: "${FLAME_HOME}/certs/server.crt"
    key_file: "${FLAME_HOME}/certs/server.key"
    ca_file: "${FLAME_HOME}/certs/ca.crt"    # Enables mTLS and authorization
    ca_key: "${FLAME_HOME}/certs/ca.key"     # For signing session certificates
    cert_validity: 24h                        # Default validity for session certs

cache:
  endpoint: "grpcs://127.0.0.1:9090"
  # ...
```

**Bootstrap Process:**

Cluster installation is done via `flmadm install`. By default, it sets up the cluster without mTLS. Use `--with-mtls` to enable mTLS and generate certificates:

```bash
# Basic install (no mTLS, no authorization)
flmadm install

# Install with mTLS enabled (generates all certs)
flmadm install --with-mtls

# With --with-mtls, this command:
# 1. Generates CA cert/key (ca.crt, ca.key)
# 2. Generates server cert/key signed by CA (server.crt, server.key)
# 3. Generates root cert/key signed by CA (root.crt, root.key)
# 4. Creates root role in database (permissions: *:*, workspaces: *)
# 5. Creates root user in database with root role
# 6. Generates flame-executor cert/key for internal components (flame-executor.crt, flame-executor.key)
# 7. Creates flame-executor user with root role

# Output structure when --with-mtls is used (default: ${FLAME_HOME}/certs/):
#   ${FLAME_HOME}/certs/
#   ├── ca.crt              # CA certificate (distribute to clients)
#   ├── ca.key              # CA private key (keep secure on server)
#   ├── server.crt          # Server certificate
#   ├── server.key          # Server private key
#   ├── root.crt            # Root user certificate
#   ├── root.key            # Root user private key
#   ├── flame-executor.crt  # Executor manager certificate
#   └── flame-executor.key  # Executor manager private key

# Root user (created with --with-mtls):
# - name: root
# - role: root
# - Full access to all workspaces and permissions
```

**RBAC Data Flow:**

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                         RBAC Management Architecture                         │
└─────────────────────────────────────────────────────────────────────────────┘

                        ┌─────────────────────────┐
                        │       flmadm CLI        │
                        │  (Admin operations)     │
                        └───────────┬─────────────┘
                                    │ gRPC (Admin service)
                                    │ mTLS with admin cert
                                    ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                          Session Manager                                     │
│  ┌─────────────────────────────────────────────────────────────────────┐   │
│  │                        Admin Service                                  │   │
│  │  - CreateUser, UpdateUser, DeleteUser, ListUsers                     │   │
│  │  - CreateRole, UpdateRole, DeleteRole, ListRoles                     │   │
│  └─────────────────────────────────────────────────────────────────────┘   │
│                                    │                                        │
│                                    ▼                                        │
│  ┌─────────────────────────────────────────────────────────────────────┐   │
│  │                      RBAC Storage (storage.Engine)                   │   │
│  │                                                                      │   │
│  │  RBAC methods are added to the existing storage::Engine trait:       │   │
│  │                                                                      │   │
│  │  // User operations                                                  │   │
│  │  async fn get_user_by_cn(&self, cn: &str) -> Result<Option<User>>;   │   │
│  │  async fn get_user_roles(&self, name: &str) -> Result<Vec<Role>>;    │   │
│  │  async fn create_user(&self, user: &User) -> Result<User>;           │   │
│  │  async fn update_user(&self, user: &User, assign: &[String], revoke: &[String]) -> Result<User>; │   │
│  │  async fn delete_user(&self, name: &str) -> Result<()>;              │   │
│  │  async fn find_users(&self, filter: Option<&str>) -> Result<Vec<User>>;│   │
│  │                                                                      │   │
│  │  // Role operations                                                  │   │
│  │  async fn get_role(&self, name: &str) -> Result<Option<Role>>;       │   │
│  │  async fn create_role(&self, role: &Role) -> Result<Role>;           │   │
│  │  async fn update_role(&self, role: &Role) -> Result<Role>;           │   │
│  │  async fn delete_role(&self, name: &str) -> Result<()>;              │   │
│  │  async fn find_roles(&self, filter: Option<&str>) -> Result<Vec<Role>>;│   │
│  │                                                                      │   │
│  │  // Workspace operations                                             │   │
│  │  async fn get_workspace(&self, name: &str) -> Result<Option<Workspace>>;│   │
│  │  async fn create_workspace(&self, ws: &Workspace) -> Result<Workspace>;│   │
│  │  async fn update_workspace(&self, ws: &Workspace) -> Result<Workspace>;│   │
│  │  async fn delete_workspace(&self, name: &str) -> Result<()>;         │   │
│  │  async fn find_workspaces(&self) -> Result<Vec<Workspace>>;          │   │
│  │                                                                      │   │
│  └─────────────────────────────────────────────────────────────────────┘   │
│                                    │                                        │
│                                    ▼                                        │
│  ┌─────────────────────────────────────────────────────────────────────┐   │
│  │                    Authorization Interceptor                         │   │
│  │  - Uses storage.Engine for RBAC lookups (same Engine as other data) │   │
│  │  - Evaluates permissions on each request                            │   │
│  └─────────────────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────────────────┘
```

**Initial Setup Example:**

```bash
# 1. Install cluster with mTLS (generates all certs and root user to ${FLAME_HOME}/certs/)
flmadm install --with-mtls

# 2. Create workspaces
flmadm create --workspace team-a --description "Team A workspace"
flmadm create --workspace team-b --description "Team B workspace"
flmadm create --workspace shared --description "Shared resources"

# 3. Create roles with permissions AND workspaces
flmadm create --role admin \
  --description "Cluster administrator" \
  --permission "*:*" \
  --workspace "*"

flmadm create --role developer \
  --description "Developer role" \
  --permission "application:*" \
  --permission "session:*" \
  --workspace team-a \
  --workspace shared

flmadm create --role data-scientist \
  --description "Data scientist role" \
  --permission "session:*" \
  --permission "application:read" \
  --workspace experiments \
  --workspace datasets

# 4. Create users and assign roles
flmadm create --user alice \
  --display-name "Alice Smith" \
  --cert-dir .
flmadm update --user alice --assign-role developer,data-scientist

flmadm create --user bob \
  --display-name "Bob Jones" \
  --cert-dir .
flmadm update --user bob --assign-role developer

# 5. Generated certificates are in current directory:
#    alice.crt, alice.key, bob.crt, bob.key
```

**Effective Permissions Calculation:**

A user's effective permissions are the union of all permissions from all assigned roles, scoped to the union of all workspaces:

```
User "alice" has roles: [developer, data-scientist]

developer role:
  permissions: [application:*, session:*]
  workspaces: [team-a, shared]

data-scientist role:
  permissions: [session:*, application:read]
  workspaces: [experiments, datasets]

Effective for alice:
  permissions: [application:*, session:*]  # Union of permissions
  workspaces: [team-a, shared, experiments, datasets]       # Union of workspaces
```

**flmadm Environment Variables:**

| Variable           | Description                  | Example                 |
| ------------------ | ---------------------------- | ----------------------- |
| `FLAME_ADMIN_CERT` | Admin certificate for flmadm | `/etc/flame/admin.crt`  |
| `FLAME_ADMIN_KEY`  | Admin private key            | `/etc/flame/admin.key`  |
| `FLAME_CA_FILE`    | CA certificate               | `/etc/flame/ca.crt`     |
| `FLAME_ENDPOINT`   | Session manager endpoint     | `https://flame-sm:8080` |

### 2.8 Cache Access for Flame Instances

Flame executors need to access the Object Cache to retrieve task inputs and store outputs. With mTLS enabled, executors must authenticate.

**Executor Manager Certificate:**

The Executor Manager uses a dedicated certificate for internal operations. The `flame-executor` user is created during cluster bootstrap with the `root` role:

```yaml
# On executor manager nodes (/etc/flame/flame-cluster.yaml)
cluster:
  endpoint: "https://flame-session-manager:8080"
  tls:
    ca_file: "/etc/flame/certs/ca.crt"
    cert_file: "/etc/flame/certs/executor-manager.crt"
    key_file: "/etc/flame/certs/executor-manager.key"
```

**Bootstrap Setup:**

During `flmadm install --with-mtls`, the following internal user is created automatically:

```bash
# Automatically created during 'flmadm install --with-mtls'
# User: flame-executor
# Role: root
# Certificate: ${FLAME_HOME}/certs/flame-executor.crt
```

### 2.9 Instance Access Model (Session-Scoped Certificates)

Both clients and instances use **mTLS for direct access** to Session Manager and Object Cache. The key difference is the certificate type:

- **Clients** use long-lived certificates with policy-based workspace access
- **Instances** use short-lived session-scoped certificates with automatic workspace/session binding

```
┌─────────────────────────────────────────────────────────────────────────┐
│                    Unified mTLS Access Model                             │
└─────────────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────┐      ┌─────────────────────────────────┐
│        Client Access            │      │         Instance Access          │
│    (SDK outside executor)       │      │     (SDK inside executor)        │
└─────────────────────────────────┘      └─────────────────────────────────┘
              │                                        │
              │ mTLS                                   │ mTLS
              │ Cert: CN=alice                         │ Cert: CN=session:abc123
              │ (long-lived, policy-based)             │ (short-lived, session-scoped)
              │                                        │
              ▼                                        ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                    Session Manager / Object Cache                        │
│                                                                         │
│  Unified Authorization:                                                 │
│  1. Extract CN from certificate                                         │
│  2. If CN starts with "session:" → session-scoped authorization         │
│     - Extract session_id, workspace from certificate                    │
│     - Validate request matches session scope                            │
│  3. Otherwise → policy-based authorization                              │
│     - Lookup subject in authorization policies                          │
│     - Validate workspace is in permitted list                           │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

**Session-Scoped Certificate Flow:**

```
┌─────────────────────────────────────────────────────────────────────────┐
│               Session-Scoped Certificate Issuance                        │
└─────────────────────────────────────────────────────────────────────────┘

1. Client creates session
   Client ──[mTLS: CN=alice]──► Session Manager
                                      │
                                      ▼
2. Session Manager creates session and generates certificate
   ┌─────────────────────────────────────────────────────┐
   │  Certificate:                                        │
   │    Subject: CN=session:abc123                        │
   │    Issuer: CN=flame-ca                               │
   │    Validity: Not After = session TTL                 │
   │                                                      │
   │    X509v3 Subject Alternative Name:                  │
   │      URI:flame://workspace/team-a                    │
   │      URI:flame://subject/abc123                      │
   │      URI:flame://parent/alice  (or parent session)   │
   └─────────────────────────────────────────────────────┘
                                      │
                                      ▼
3. Session scheduled to Executor Manager
   Session Manager ──► Executor Manager
                       (session context includes cert + key)
                                      │
                                      ▼
4. Executor spawns Instance with certificate
   Executor Manager ──► Instance
                        ENV:
                          FLAME_SESSION_CERT=/tmp/flame/session.crt
                          FLAME_SESSION_KEY=/tmp/flame/session.key
                          FLAME_CA_FILE=/etc/flame/ca.crt
                          FLAME_SESSION_ID=abc123
                          FLAME_WORKSPACE=team-a
                                      │
                                      ▼
5. Instance uses session cert for direct mTLS access
   Instance ──[mTLS: CN=session:abc123]──► Session Manager
   Instance ──[mTLS: CN=session:abc123]──► Object Cache
```

**Session Certificate Structure:**

```
Certificate:
  Version: 3
  Serial Number: <unique>
  
  Signature Algorithm: ECDSA with SHA-256
  
  Issuer: CN=flame-ca, O=Flame
  
  Validity:
    Not Before: 2026-03-31 10:00:00 UTC
    Not After:  2026-03-31 22:00:00 UTC  # Matches session TTL
  
  Subject: CN=session:abc123
  
  X509v3 Extensions:
    X509v3 Basic Constraints: critical
      CA: FALSE
    
    X509v3 Key Usage: critical
      Digital Signature
    
    X509v3 Extended Key Usage:
      TLS Web Client Authentication
    
    X509v3 Subject Alternative Name:
      URI:flame://workspace/team-a
      URI:flame://subject/abc123
      URI:flame://parent/alice
```

**Authorization Logic:**

```rust
// common/src/authz.rs

pub async fn authorize_request(
    cert: &Certificate,
    requested_workspace: &str,
    engine: &dyn Engine,  // Uses existing storage.Engine trait
) -> Result<AuthzContext, AuthzError> {
    let cn = extract_cn(cert)?;
    
    if cn.starts_with("session:") || cn.starts_with("delegate:") {
        // Session-scoped or delegation certificate authorization
        authorize_session_cert(cert, requested_workspace)
    } else {
        // RBAC-based authorization (users)
        authorize_by_rbac(&cn, requested_workspace, engine).await
    }
}

fn authorize_session_cert(
    cert: &Certificate,
    requested_workspace: &str,
) -> Result<AuthzContext, AuthzError> {
    // Extract session info from certificate SANs
    let subject = extract_san_uri(cert, "flame://subject/")?;
    let workspace = extract_san_uri(cert, "flame://workspace/")?;
    let parent = extract_san_uri(cert, "flame://parent/").ok(); // Optional
    
    // Session cert can only access its own workspace
    if requested_workspace != workspace {
        return Err(AuthzError::PermissionDenied(format!(
            "session cert for workspace '{}' cannot access workspace '{}'",
            workspace, requested_workspace
        )));
    }
    
    Ok(AuthzContext {
        subject,
        workspace,
        scope: CredentialScope::Session,
    })
}

async fn authorize_by_rbac(
    subject: &str,
    requested_workspace: &str,
    engine: &dyn Engine,  // Uses existing storage.Engine trait
) -> Result<AuthzContext, AuthzError> {
    // Lookup user by certificate CN
    let user = engine.get_user_by_cn(subject).await?
        .ok_or_else(|| AuthzError::UserNotFound(subject.to_string()))?;
    
    if !user.enabled {
        return Err(AuthzError::UserDisabled(subject.to_string()));
    }
    
    // Get user's roles and check if any grants access to the workspace
    let roles = engine.get_user_roles(&user.name).await?;
    let has_access = roles.iter().any(|r| r.has_workspace(requested_workspace));
    
    if !has_access {
        return Err(AuthzError::PermissionDenied(format!(
            "user '{}' not authorized for workspace '{}'",
            subject, requested_workspace
        )));
    }
    
    Ok(AuthzContext {
        subject: subject.to_string(),
        workspace: requested_workspace.to_string(),
        scope: CredentialScope::User,
    })
}
```

**Session Manager Certificate Issuance:**

```rust
// session_manager/src/controller/mod.rs

impl Controller {
    async fn create_session(&self, req: CreateSessionRequest, authz: &AuthzContext) -> Result<Session, FlameError> {
        let session = Session {
            id: generate_session_id(),
            workspace: authz.workspace.clone(),
            application: req.application,
            // ...
        };
        
        // Generate session-scoped certificate
        // Parent in cert is authz.subject (for credential chain validation)
        let session_cert = self.ca.issue_session_certificate(
            &session.id,
            &session.workspace,
            &authz.subject,  // Parent for credential chain
            session.ttl,
        )?;
        
        // Store session with certificate
        self.storage.create_session(&session, &session_cert).await?;
        
        Ok(session)
    }
}

// common/src/ca.rs

pub struct FlameCA {
    ca_cert: Certificate,
    ca_key: PrivateKey,
}

impl FlameCA {
    pub fn issue_session_certificate(
        &self,
        session_id: &str,
        workspace: &str,
        parent: &str,
        ttl: Duration,
    ) -> Result<SessionCertificate, FlameError> {
        // Generate key pair for session
        let key = PrivateKey::generate_ec(EcCurve::P256)?;
        
        // Build certificate
        let cert = CertificateBuilder::new()
            .common_name(&format!("session:{}", session_id))
            .add_san_uri(&format!("flame://workspace/{}", workspace))
            .add_san_uri(&format!("flame://subject/{}", session_id))
            .add_san_uri(&format!("flame://parent/{}", parent))
            .not_before(Utc::now())
            .not_after(Utc::now() + ttl)
            .key_usage(KeyUsage::DigitalSignature)
            .extended_key_usage(ExtendedKeyUsage::ClientAuth)
            .basic_constraints(false) // Not a CA
            .sign(&self.ca_key, &self.ca_cert)?;
        
        Ok(SessionCertificate {
            cert_pem: cert.to_pem()?,
            key_pem: key.to_pem()?,
        })
    }
}
```

**Instance Environment Variables:**

| Variable                | Description                 | Example                    |
| ----------------------- | --------------------------- | -------------------------- |
| `FLAME_SESSION_CERT`    | Path to session certificate | `/tmp/flame/session.crt`   |
| `FLAME_SESSION_KEY`     | Path to session private key | `/tmp/flame/session.key`   |
| `FLAME_CA_FILE`         | Path to CA certificate      | `/etc/flame/ca.crt`        |
| `FLAME_SESSION_ID`      | Session ID (convenience)    | `abc123`                   |
| `FLAME_WORKSPACE`       | Workspace (convenience)     | `team-a`                   |
| `FLAME_SESSION_MANAGER` | Session Manager endpoint    | `https://flame-sm:8080`    |
| `FLAME_CACHE_ENDPOINT`  | Object Cache endpoint       | `grpcs://flame-cache:9090` |

**SDK Changes (Instance Mode):**

```python
# Python SDK - flamepy/core/client.py

class FlameClient:
    """Flame client that works in both client and instance mode."""
    
    def __init__(self):
        # Detect mode based on environment
        self._session_cert = os.getenv("FLAME_SESSION_CERT")
        
        if self._session_cert:
            # Instance mode: use session certificate
            self._tls_config = TlsConfig(
                ca_file=os.getenv("FLAME_CA_FILE"),
                cert_file=self._session_cert,
                key_file=os.getenv("FLAME_SESSION_KEY"),
            )
            self._session_id = os.getenv("FLAME_SESSION_ID")
            self._workspace = os.getenv("FLAME_WORKSPACE")
        else:
            # Client mode: use config file
            config = load_flame_config()
            self._tls_config = config.tls
            self._workspace = config.workspace
            self._session_id = None
    
    def create_session(self, application: str, slots: int = 1) -> Session:
        """Create a session (or child session in instance mode)."""
        channel = self._create_channel(os.getenv("FLAME_SESSION_MANAGER") or self._config.endpoint)
        stub = FrontendStub(channel)
        
        response = stub.CreateSession(CreateSessionRequest(
            application=application,
            slots=slots,
            workspace=self._workspace,
        ))
        
        return Session.from_proto(response)
    
    def put_object(self, data: bytes) -> ObjectRef:
        """Put object to cache."""
        channel = self._create_channel(os.getenv("FLAME_CACHE_ENDPOINT") or self._config.cache_endpoint)
        # Use Arrow Flight client with mTLS
        client = FlightClient(channel)
        # ...
    
    def _create_channel(self, endpoint: str):
        """Create gRPC channel with mTLS."""
        credentials = grpc.ssl_channel_credentials(
            root_certificates=open(self._tls_config.ca_file, 'rb').read(),
            private_key=open(self._tls_config.key_file, 'rb').read(),
            certificate_chain=open(self._tls_config.cert_file, 'rb').read(),
        )
        return grpc.secure_channel(endpoint, credentials)
```

```rust
// Rust SDK
impl FlameClient {
    pub fn from_env() -> Result<Self, FlameError> {
        if let Ok(cert_path) = std::env::var("FLAME_SESSION_CERT") {
            // Instance mode
            let tls = FlameClientTls {
                ca_file: Some(std::env::var("FLAME_CA_FILE")?),
                cert_file: Some(cert_path),
                key_file: Some(std::env::var("FLAME_SESSION_KEY")?),
            };
            
            Ok(Self {
                session_manager: std::env::var("FLAME_SESSION_MANAGER")?,
                cache_endpoint: std::env::var("FLAME_CACHE_ENDPOINT")?,
                workspace: std::env::var("FLAME_WORKSPACE")?,
                tls,
            })
        } else {
            // Client mode - load from config
            Self::from_config()
        }
    }
}
```

**Security Properties:**

| Property                  | Description                                    |
| ------------------------- | ---------------------------------------------- |
| **Scope limitation**      | Session cert can only access its own workspace |
| **Automatic expiry**      | Certificate validity matches session TTL       |
| **No revocation needed**  | Short-lived certs expire naturally             |
| **Workspace inheritance** | Child sessions inherit parent's workspace      |
| **Single auth mechanism** | mTLS everywhere, no tokens                     |
| **Direct access**         | No proxy bottleneck                            |

**Comparison with Executor Proxy:**

| Aspect                 | Executor Proxy               | Session-Scoped Cert        |
| ---------------------- | ---------------------------- | -------------------------- |
| Auth mechanism         | mTLS (single)                | mTLS (single)              |
| Network path           | Instance → Executor → Target | Instance → Target (direct) |
| Latency                | +0.1-1ms per op              | Direct                     |
| Credential in instance | None                         | Certificate files          |
| Executor load          | High (proxies all)           | None                       |
| Certificate management | None                         | Generate per session       |
| Instance compromise    | Unix socket access           | Session-scoped access      |

### 2.10 Session Certificate Lifecycle

**Certificate Issuance:**

Session certificates are generated by Session Manager when a session is created:

1. Client requests session creation via mTLS
2. Session Manager validates client authorization for workspace
3. Session Manager generates session-scoped certificate (signed by Flame CA)
4. Certificate + key returned in session response
5. Executor Manager receives certificate when session is scheduled
6. Executor passes certificate to Instance via environment/files

**Certificate Renewal:**

Session certificates have the same TTL as the session. For long-running sessions:

- Session Manager can issue renewal certificates before expiry
- Executor Manager monitors certificate expiry and requests renewal
- Instance receives updated certificate via file or signal

**Certificate Revocation:**

No explicit revocation needed due to short-lived certificates:

- Certificate expires when session ends
- If session is terminated early, certificate becomes invalid at next authorization check
- Session Manager tracks active sessions; terminated sessions fail authorization

**Nested Sessions:**

When an Instance creates a child session:

1. Instance connects to Session Manager with session certificate (`CN=session:parent123`)
2. Session Manager validates parent session is active
3. Child session inherits workspace from parent (enforced, cannot override)
4. Child session certificate includes parent reference in SAN
5. Child session TTL cannot exceed parent session TTL

```
Parent Session: abc123 (workspace: team-a, TTL: 12h)
    │
    ├── Child Session: def456 (workspace: team-a, TTL: ≤12h)
    │       │
    │       └── Grandchild: ghi789 (workspace: team-a, TTL: ≤parent)
    │
    └── Child Session: jkl012 (workspace: team-a, TTL: ≤12h)
```

**Authorization Rules for Session Certificates:**

| Operation                              | Allowed | Notes                                       |
| -------------------------------------- | ------- | ------------------------------------------- |
| Access own session's cache objects     | ✓       | Key must match `{workspace}/{session_id}/*` |
| Access parent session's cache objects  | ✓       | If parent SAN present                       |
| Access sibling session's cache objects | ✗       | Different session tree                      |
| Create child session                   | ✓       | Inherits workspace                          |
| Create session in different workspace  | ✗       | Workspace locked to cert                    |
| Close own session                      | ✓       |                                             |
| Close child session                    | ✓       |                                             |
| Close parent session                   | ✗       |                                             |

### 2.11 Credential Delegation for Executors

The session manager signs credentials for executors based on the session's `Credential` specification. These credentials are passed to instances via their working directory and environment variables.

**Credential Flow:**

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                     Credential Delegation Flow                               │
└─────────────────────────────────────────────────────────────────────────────┘

1. Client creates session with credential specification
   ┌─────────────────────────────────────────────────────────────────────────┐
   │  CreateSessionRequest {                                                  │
   │    session_id: "train-001",                                              │
   │    workspace: "team-a",                                                  │
   │    session: {                                                            │
   │      application: "ml-training",                                         │
   │      credential: {                                                       │
   │        user: "alice",           // Must match client cert CN (or empty)  │
   │        scope: CREDENTIAL_SCOPE_USER  // or CREDENTIAL_SCOPE_SESSION      │
   │      }                                                                   │
   │    }                                                                     │
   │  }                                                                       │
   └─────────────────────────────────────────────────────────────────────────┘
                                    │
                                    ▼
2. Session Manager validates and signs credential
   ┌─────────────────────────────────────────────────────────────────────────┐
   │  Session Manager:                                                        │
   │  - Validates client cert CN matches credential.user (if specified)       │
   │  - Generates delegation certificate based on scope:                      │
   │                                                                         │
│  CREDENTIAL_SCOPE_USER:                                                 │
│    → Signs delegation cert with user's permissions                       │
│    → CN=delegate:{user}:{session_id}                                     │
│    → SAN: flame://subject/{user}, flame://workspace/{workspace}          │
│    → Full user permissions in workspace                                  │
   │                                                                         │
   │  CREDENTIAL_SCOPE_SESSION:                                              │
   │    → Signs session-scoped cert (existing behavior)                       │
   │    → CN=session:{session_id}                                             │
   │    → Only access to this session's resources (cache, child sessions)     │
   └─────────────────────────────────────────────────────────────────────────┘
                                    │
                                    ▼
3. Executor Manager receives session context with signed credential
   ┌─────────────────────────────────────────────────────────────────────────┐
   │  SessionContext {                                                        │
   │    session_id: "train-001",                                              │
   │    workspace: "team-a",                                                  │
   │    credential: {                                                         │
   │      cert_pem: "-----BEGIN CERTIFICATE-----...",                         │
   │      key_pem: "-----BEGIN PRIVATE KEY-----...",                          │
   │      scope: CREDENTIAL_SCOPE_USER,                                       │
   │    }                                                                     │
   │  }                                                                       │
   └─────────────────────────────────────────────────────────────────────────┘
                                    │
                                    ▼
4. Executor writes credentials to instance working directory and exports env
   ┌─────────────────────────────────────────────────────────────────────────┐
   │  Instance Working Directory:                                             │
   │    /workdir/                                                             │
   │    ├── .flame/                                                           │
   │    │   ├── credential.crt    # Delegation certificate                    │
   │    │   ├── credential.key    # Private key                               │
   │    │   └── ca.crt            # CA certificate for verification           │
   │    └── ... (application files)                                           │
   │                                                                         │
   │  Environment Variables:                                                  │
   │    FLAME_CREDENTIAL_CERT=/workdir/.flame/credential.crt                  │
   │    FLAME_CREDENTIAL_KEY=/workdir/.flame/credential.key                   │
   │    FLAME_CA_FILE=/workdir/.flame/ca.crt                                  │
   │    FLAME_CREDENTIAL_SCOPE=user  # or "session"                           │
   │    FLAME_CREDENTIAL_USER=alice  # user associated with credential        │
   │    FLAME_SESSION_ID=train-001                                            │
   │    FLAME_WORKSPACE=team-a                                                │
   │    FLAME_SESSION_MANAGER=https://flame-sm:8080                           │
   │    FLAME_CACHE_ENDPOINT=grpcs://flame-cache:9090                         │
   └─────────────────────────────────────────────────────────────────────────┘
                                    │
                                    ▼
5. Instance uses credentials for direct mTLS access
   ┌─────────────────────────────────────────────────────────────────────────┐
   │  Instance code reads credentials from environment and files:              │
   │                                                                         │
   │  # Python SDK detects credentials automatically                          │
   │  client = FlameClient()  # Auto-detects FLAME_CREDENTIAL_* env vars      │
   │                                                                         │
   │  # With CREDENTIAL_SCOPE_USER:                                           │
   │  # - Can create new sessions in workspace                                │
   │  # - Can access any cache object in workspace                            │
   │  # - Has full user permissions delegated from session creator            │
   │                                                                         │
   │  # With CREDENTIAL_SCOPE_SESSION:                                        │
   │  # - Can only create child sessions                                      │
   │  # - Can only access this session's cache objects                        │
   │  # - Limited to session-specific operations                              │
   └─────────────────────────────────────────────────────────────────────────┘
```

**Credential Scope Comparison:**

| Aspect              | `CREDENTIAL_SCOPE_USER`           | `CREDENTIAL_SCOPE_SESSION` |
| ------------------- | --------------------------------- | -------------------------- |
| Certificate CN      | `delegate:{user}:{session_id}`    | `session:{session_id}`     |
| Workspace access    | Full user permissions             | Session workspace only     |
| Cache access        | All objects in workspace          | Session's objects only     |
| Create sessions     | Yes, new sessions                 | Only child sessions        |
| Create applications | Yes (if user has permission)      | No                         |
| Use case            | Agents needing user-level actions | Isolated task execution    |

**Credential Validation Rules:**

| Rule             | Description                                                  |
| ---------------- | ------------------------------------------------------------ |
| User match       | If `credential.user` is specified, must match client cert CN |
| User optional    | If `credential.user` is empty, inherits from client cert     |
| Scope default    | If `credential.scope` is unspecified, defaults to `SESSION`  |
| Permission check | User must have permissions in target workspace               |
| TTL inheritance  | Credential cert TTL matches session TTL                      |

**Certificate Structure for User-Scoped Delegation:**

```
Certificate:
  Version: 3
  Serial Number: <unique>
  
  Signature Algorithm: ECDSA with SHA-256
  
  Issuer: CN=flame-ca, O=Flame
  
  Validity:
    Not Before: 2026-03-31 10:00:00 UTC
    Not After:  2026-03-31 22:00:00 UTC  # Matches session TTL
  
  Subject: CN=delegate:alice:train-001
  
  X509v3 Extensions:
    X509v3 Basic Constraints: critical
      CA: FALSE
    
    X509v3 Key Usage: critical
      Digital Signature
    
    X509v3 Extended Key Usage:
      TLS Web Client Authentication
    
    X509v3 Subject Alternative Name:
      URI:flame://subject/alice
      URI:flame://workspace/team-a
      URI:flame://parent/train-001
      URI:flame://scope/user
```

**SDK Support:**

```python
# Python SDK - flamepy/core/client.py

class FlameClient:
    """Flame client with automatic credential detection."""
    
    def __init__(self):
        # Check for delegation credentials first
        self._credential_cert = os.getenv("FLAME_CREDENTIAL_CERT")
        
        if self._credential_cert:
            # Instance mode with delegated credentials
            self._tls_config = TlsConfig(
                ca_file=os.getenv("FLAME_CA_FILE"),
                cert_file=self._credential_cert,
                key_file=os.getenv("FLAME_CREDENTIAL_KEY"),
            )
            self._scope = os.getenv("FLAME_CREDENTIAL_SCOPE", "session")
            self._user = os.getenv("FLAME_CREDENTIAL_USER")
            self._session_id = os.getenv("FLAME_SESSION_ID")
            self._workspace = os.getenv("FLAME_WORKSPACE")
        elif os.getenv("FLAME_SESSION_CERT"):
            # Legacy session cert mode (backward compatibility)
            self._tls_config = TlsConfig(
                ca_file=os.getenv("FLAME_CA_FILE"),
                cert_file=os.getenv("FLAME_SESSION_CERT"),
                key_file=os.getenv("FLAME_SESSION_KEY"),
            )
            self._scope = "session"
            self._session_id = os.getenv("FLAME_SESSION_ID")
            self._workspace = os.getenv("FLAME_WORKSPACE")
        else:
            # Client mode - load from config file
            config = load_flame_config()
            self._tls_config = config.tls
            self._workspace = config.workspace
```

```rust
// Rust SDK
impl FlameClient {
    pub fn from_env() -> Result<Self, FlameError> {
        // Check for delegated credentials first
        if let Ok(cert_path) = std::env::var("FLAME_CREDENTIAL_CERT") {
            let tls = FlameClientTls {
                ca_file: Some(std::env::var("FLAME_CA_FILE")?),
                cert_file: Some(cert_path),
                key_file: Some(std::env::var("FLAME_CREDENTIAL_KEY")?),
            };
            
            return Ok(Self {
                session_manager: std::env::var("FLAME_SESSION_MANAGER")?,
                cache_endpoint: std::env::var("FLAME_CACHE_ENDPOINT")?,
                workspace: std::env::var("FLAME_WORKSPACE")?,
                credential_scope: std::env::var("FLAME_CREDENTIAL_SCOPE")
                    .unwrap_or_else(|_| "session".to_string()),
                tls,
            });
        }
        
        // Fall back to legacy session cert or config file
        Self::from_legacy_env_or_config()
    }
}
```

### 2.12 Certificate Manager (Internal)

Flame uses a CertManager interface **internal to the session-manager** to handle certificate operations. This is a Rust trait, not a gRPC service.

**Note:** This interface is NOT exposed via gRPC. It is an internal abstraction used by the Session Manager to manage session certificate signing and verification.

**CertManager Trait:**

```rust
// session_manager/src/cert/manager.rs

use async_trait::async_trait;

/// CertManager defines the internal interface for certificate management.
/// This is NOT a gRPC service - it's an internal trait used by Session Manager.
#[async_trait]
pub trait CertManager: Send + Sync {
    /// Issue a credential for an executor/session.
    /// Creates a new certificate scoped to the session/user based on the parent credential.
    async fn issue(
        &self,
        request: IssueRequest,
    ) -> Result<SessionCredential, CertError>;
    
    /// Verify the signature and validity of a credential.
    /// Returns the verified claims (subject, workspace, scope, etc.) if valid.
    async fn verify(
        &self,
        credential: &[u8],
    ) -> Result<VerifiedClaims, CertError>;
}

/// Request to issue credentials to an executor
pub struct IssueRequest {
    pub parent: String,               // Parent subject (user CN or parent session ID)
    pub parent_scope: CredentialScope, // Parent's scope (for validation)
    pub subject: String,              // Subject for the new credential (session_id for SESSION scope, user CN for USER scope)
    pub workspace: String,            // Workspace the credential is valid for
    pub scope: CredentialScope,       // USER or SESSION scope
    pub ttl: Duration,                // Time-to-live for the credential
}

/// Verified claims extracted from a credential
pub struct VerifiedClaims {
    pub subject: String,              // Subject (session_id for SESSION scope, user CN for USER scope)
    pub parent: Option<String>,       // Parent subject (user CN or parent session ID)
    pub workspace: String,            // Workspace from credential
    pub scope: CredentialScope,       // USER or SESSION
    pub expires_at: SystemTime,       // Expiration time
}

/// Session credential returned by issue operation
pub struct SessionCredential {
    pub certificate: Vec<u8>,        // PEM-encoded certificate
    pub private_key: Vec<u8>,        // PEM-encoded private key
    pub ca_certificate: Vec<u8>,     // PEM-encoded CA certificate
    pub expires_at: SystemTime,      // Expiration time
}
```

**CertManager Implementation:**

```rust
// session_manager/src/cert/manager.rs

pub struct CertManagerImpl {
    ca_cert: Certificate,
    ca_key: PrivateKey,
    ca_chain: Vec<Certificate>,
}

#[async_trait]
impl CertManager for CertManagerImpl {
    async fn issue(
        &self,
        request: IssueRequest,
    ) -> Result<SessionCredential, CertError> {
        // Validate: child scope cannot exceed parent scope
        // Scope hierarchy: USER > SESSION (USER can do more than SESSION)
        // - Parent USER can issue USER or SESSION
        // - Parent SESSION can only issue SESSION
        if request.parent_scope == CredentialScope::Session 
            && request.scope == CredentialScope::User {
            return Err(CertError::ScopeEscalation(
                "SESSION-scoped parent cannot issue USER-scoped credential".to_string()
            ));
        }
        
        // Generate new key pair for the credential
        let key = PrivateKey::generate_ec(EcCurve::P256)?;
        
        // Build certificate based on scope
        let cn = match request.scope {
            CredentialScope::User => format!("delegate:{}:{}", request.parent, request.subject),
            CredentialScope::Session => format!("session:{}", request.subject),
        };
        
        let cert = CertificateBuilder::new()
            .common_name(&cn)
            .add_san_uri(&format!("flame://workspace/{}", request.workspace))
            .add_san_uri(&format!("flame://subject/{}", request.subject))
            .add_san_uri(&format!("flame://parent/{}", request.parent))
            .add_san_uri(&format!("flame://scope/{}", request.scope.as_str()))
            .not_before(SystemTime::now())
            .not_after(SystemTime::now() + request.ttl)
            .key_usage(KeyUsage::DigitalSignature)
            .extended_key_usage(ExtendedKeyUsage::ClientAuth)
            .sign(&self.ca_key, &self.ca_cert)?;
        
        Ok(SessionCredential {
            certificate: cert.to_pem()?,
            private_key: key.to_pem()?,
            ca_certificate: self.ca_cert.to_pem()?,
            expires_at: SystemTime::now() + request.ttl,
        })
    }
    
    async fn verify(
        &self,
        credential: &[u8],
    ) -> Result<VerifiedClaims, CertError> {
        // Parse and verify certificate
        let cert = Certificate::from_pem(credential)?;
        
        // Verify signature chain
        cert.verify_chain(&self.ca_chain)?;
        
        // Check expiration
        if cert.not_after() < SystemTime::now() {
            return Err(CertError::CredentialExpired);
        }
        
        // Extract claims from certificate
        let workspace = extract_san_uri(&cert, "flame://workspace/")?;
        let subject = extract_san_uri(&cert, "flame://subject/")?;
        let scope = extract_san_uri(&cert, "flame://scope/")?
            .parse::<CredentialScope>()?;
        let parent = extract_san_uri(&cert, "flame://parent/").ok();
        
        Ok(VerifiedClaims {
            subject,
            parent,
            workspace,
            scope,
            expires_at: cert.not_after(),
        })
    }
}
```

**Usage in Session Manager:**

```rust
// session_manager/src/controller/mod.rs

impl Controller {
    pub fn new(cert_manager: Arc<dyn CertManager>) -> Self {
        Self { cert_manager, /* ... */ }
    }
    
    async fn create_session(
        &self,
        req: CreateSessionRequest,
        authz: &AuthzContext,
    ) -> Result<Session, FlameError> {
        let session = Session {
            id: generate_session_id(),
            workspace: authz.workspace.clone(),
            // ...
        };
        
        // Determine credential scope from request
        let scope = req.session.credential
            .map(|c| c.scope)
            .unwrap_or(CredentialScope::Session);
        
        // Use cert manager to issue credential for executor
        // CertManager validates that child scope doesn't exceed parent scope
        let credential = self.cert_manager.issue(IssueRequest {
            parent: authz.subject.clone(),
            parent_scope: authz.scope,  // Parent's scope for validation
            subject: session.id.clone(),
            workspace: session.workspace.clone(),
            scope,
            ttl: session.ttl,
        }).await?;
        
        // Store session with credential
        self.storage.create_session(&session, &credential).await?;
        
        Ok(session)
    }
}
```

### 2.13 Scope

**In Scope:**

- Workspace field in all object metadata (Application, Session, Task, Node, Executor) - optional for User, Role, Workspace
- Pre-defined workspaces: `default` (client resources), `system` (internal components)
- Workspace CRUD operations via Frontend service (`flmctl create/list/get/update/delete workspace`)
- mTLS client authentication in Session Manager and Object Cache
- **CertManager (Internal to Session Manager):**
  - `CertManager` trait for certificate operations - NOT a gRPC service
  - Core operations: `issue`, `verify`
- **RBAC (Role-Based Access Control):**
  - Role definitions with permissions and workspace assignments
  - Permission types: `application:*`, `session:*`, `workspace:*`
  - A role can manage multiple workspaces
  - A user can have multiple roles
- **User/Role Management (Admin service via `flmadm`):**
  - User CRUD with automatic certificate generation (`flmadm create/list/get/update/delete --user`)
  - Role CRUD with permission and workspace configuration (`flmadm create/list/get/update/delete --role`)
  - Role assignment/revocation via user update (`flmadm update --user --assign-role/--revoke-role`)
- **Session Credentials:**
  - Credential field in SessionSpec with user and scope
  - `CREDENTIAL_SCOPE_USER`: Delegation cert with full user permissions in workspace
  - `CREDENTIAL_SCOPE_SESSION`: Session-scoped cert with limited access
  - Executor writes credentials to instance working directory (`.flame/`)
  - Environment variables exported for SDK auto-detection
- Session-scoped certificates for instances:
  - Session Manager generates short-lived certificates per session
  - Certificates encode workspace and session scope
  - Instances use session certificates for direct mTLS access
  - Session certs inherit permissions from creating subject
- Nested session support with workspace inheritance
- CLI workspace support (verb + noun format)
- Backward compatibility: resources without workspace default to `default`
- **RBAC stored via storage.Engine (not separate trait):**
  - Users, roles, workspaces, and bindings persisted via existing storage backend
  - Dynamic updates via Admin API (no restart required)
  - Both SQLite and filesystem backends support RBAC

**Out of Scope:**

- Executor Proxy for cache/session operations (using session certificates instead)
- Workspace quotas or resource limits
- Audit logging (future enhancement)
- Per-resource ACLs (permissions are at resource-type level)
- Static RBAC configuration in YAML files (use Admin API instead)

**Limitations:**

- Workspace is immutable after resource creation
- No hierarchical workspaces (flat namespace)
- Single CA for all certificates (no per-workspace CAs)
- `system` workspace is not accessible to regular users (internal components only)
- Session certificates require secure file handling in instances
- **Scope hierarchy enforced**: USER > SESSION. A SESSION-scoped parent cannot issue USER-scoped credentials
- Delegation certs (CREDENTIAL_SCOPE_USER) have same permissions as creating user

### 2.14 Feature Interaction

**Related Features:**

| Feature           | Interaction                                           |
| ----------------- | ----------------------------------------------------- |
| TLS (RFE234)      | Extends TLS to mTLS with client certificates          |
| Session Manager   | Issues session-scoped certs; enforces RBAC            |
| Object Cache      | Enforces workspace isolation and RBAC via mTLS        |
| Executor Manager  | Passes session certificates to instances              |
| SDK (Rust/Python) | Unified mTLS client for both client and instance mode |

**Updates Required:**

| Component        | File                                               | Change                                                                                         |
| ---------------- | -------------------------------------------------- | ---------------------------------------------------------------------------------------------- |
| Proto            | `rpc/protos/types.proto`                           | Add optional `workspace` to `Metadata`; Add `User`, `Role`, `Workspace`, `Credential` messages |
| Proto            | `rpc/protos/frontend.proto`                        | Add `workspace` to request messages; Workspace CRUD operations                                 |
| Proto            | `rpc/protos/admin.proto`                           | NEW: Admin service for User/Role management                                                    |
| Common           | `common/src/ctx.rs`                                | Add `FlameRbac` config                                                                         |
| Common           | `common/src/rbac.rs`                               | NEW: RBAC roles, permission checking logic                                                     |
| Common           | `common/src/authz.rs`                              | NEW: Unified authorization (mTLS + RBAC using storage.Engine)                                  |
| Common           | `common/src/apis.rs`                               | Add `workspace` to all domain structs; Add `User`, `Role`, `Workspace`, `Credential` structs   |
| Session Manager  | `session_manager/src/storage/engine/mod.rs`        | Extend `Engine` trait with RBAC methods (User, Role, Workspace CRUD)                           |
| Session Manager  | `session_manager/src/storage/engine/sqlite.rs`     | Implement RBAC methods for SQLite                                                              |
| Session Manager  | `session_manager/src/storage/engine/filesystem.rs` | Implement RBAC methods for filesystem                                                          |
| Session Manager  | `session_manager/src/cert/mod.rs`                  | NEW: Certificate manager module                                                                |
| Session Manager  | `session_manager/src/cert/manager.rs`              | NEW: `CertManager` trait (issue, verify)                                                       |
| Session Manager  | `session_manager/src/apiserver/mod.rs`             | mTLS server config, authz interceptor using storage.Engine                                     |
| Session Manager  | `session_manager/src/controller/mod.rs`            | Use `CertManager` for credential issuance                                                      |
| Session Manager  | `session_manager/src/model/mod.rs`                 | Workspace filtering (uses storage.Engine for queries)                                          |
| Object Cache     | `object_cache/src/cache.rs`                        | mTLS verification, workspace-scoped keys                                                       |
| Executor Manager | `executor_manager/src/client.rs`                   | Client cert config                                                                             |
| Executor Manager | `executor_manager/src/executor.rs`                 | Pass credential cert to instance working directory and env                                     |
| SDK Rust         | `sdk/rust/src/apis/ctx.rs`                         | Client cert config; Credential detection from env                                              |
| SDK Rust         | `sdk/rust/src/client/mod.rs`                       | Unified mTLS client (detect mode from env)                                                     |
| SDK Python       | `sdk/python/src/flamepy/core/client.py`            | Unified mTLS client; Credential detection                                                      |
| CLI              | `flmctl/src/*.rs`                                  | `--workspace` flag; workspace commands (verb+noun); `--credential-*` flags                     |
| CLI              | `flmadm/src/*.rs`                                  | init, user/role commands (verb+noun format)                                                    |

**Breaking Changes:**

- Proto: `Metadata` message adds optional `workspace` field (additive, not breaking)
- API: List operations may return filtered results based on authorization

**Migration Path:**

1. Deploy with `flmadm install` (no mTLS) - no authorization enforced
2. When ready to enable mTLS, run `flmadm install --with-mtls` to generate certificates
3. Configure `tls` section in `flame-cluster.yaml` to use generated certs
4. Users and roles are managed via `flmadm` using root credentials
5. Generate client certificates for users via `flmadm create --user`
6. Clients without valid certificates will be rejected

## 3. Implementation Detail

### 3.1 Architecture

```
┌─────────────────────────────────────────────────────────────────────────┐
│                              Client                                      │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │  flame.yaml:                                                      │   │
│  │    contexts:                                                      │   │
│  │      - name: team-a                                               │   │
│  │        cluster:                                                   │   │
│  │          tls:                                                     │   │
│  │            cert_file: team-a.crt   ←── Client Certificate        │   │
│  │            key_file: team-a.key                                   │   │
│  │        workspace: team-a           ←── Default Workspace         │   │
│  └─────────────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────────────┘
              │                                           │
              │ mTLS (CN=alice)                           │ mTLS (CN=alice)
              │ + x-flame-workspace: team-a               │
              ▼                                           ▼
┌──────────────────────────────────┐    ┌──────────────────────────────────┐
│       Session Manager            │    │         Object Cache              │
│                                  │    │                                   │
│  ┌────────────┐  ┌────────────┐ │    │  ┌─────────────────────────────┐ │
│  │ TLS Layer  │─►│ Authz      │ │    │  │  TLS Layer + Authz          │ │
│  │            │  │ Intercept  │ │    │  │                             │ │
│  │ Extract CN │  │            │ │    │  │  1. Extract CN from cert    │ │
│  └────────────┘  └────────────┘ │    │  │  2. If session cert → scope │ │
│                                  │    │  │  3. Else → policy lookup    │ │
│  - Issues session certificates   │    │  └─────────────────────────────┘ │
│  - Authorization Policies        │    │                                   │
│  - Tracks session hierarchy      │    │  Key: {workspace}/{session}/{obj} │
└──────────────────────────────────┘    └──────────────────────────────────┘
              │                                           ▲
              │ Session created                           │
              │ + session certificate                     │
              ▼                                           │
┌──────────────────────────────────────────────────────────────────────────┐
│                          Executor Manager                                 │
│                                                                          │
│  Receives session context with certificate from Session Manager          │
│  Spawns Instance with session certificate in environment                 │
│                                                                          │
└──────────────────────────────────────────────────────────────────────────┘
              │
              │ Passes cert via env/files
              ▼
┌──────────────────────────────────────────────────────────────────────────┐
│                              Instance                                     │
│                                                                          │
│  ┌────────────────────────────────────────────────────────────────────┐ │
│  │  Environment:                                                       │ │
│  │    FLAME_SESSION_CERT=/tmp/flame/session.crt                        │ │
│  │    FLAME_SESSION_KEY=/tmp/flame/session.key                         │ │
│  │    FLAME_CA_FILE=/etc/flame/ca.crt                                  │ │
│  │    FLAME_SESSION_ID=abc123                                          │ │
│  │    FLAME_WORKSPACE=team-a                                           │ │
│  └────────────────────────────────────────────────────────────────────┘ │
│                                                                          │
│  Direct mTLS access using session certificate:                           │
│  - CN=session:abc123                                                     │
│  - Workspace encoded in certificate SAN                                  │
│  - Can only access own workspace                                         │
│  - Can create child sessions (inherit workspace)                         │
│                                                                          │
└──────────────────────────────────────────────────────────────────────────┘
              │                                           │
              │ mTLS (CN=session:abc123)                  │ mTLS (CN=session:abc123)
              ▼                                           ▼
┌──────────────────────────────────┐    ┌──────────────────────────────────┐
│       Session Manager            │    │         Object Cache              │
│                                  │    │                                   │
│  Session cert authorization:     │    │  Session cert authorization:      │
│  - Validate cert not expired     │    │  - Validate cert not expired      │
│  - Extract workspace from SAN    │    │  - Extract workspace from SAN     │
│  - Allow child session creation  │    │  - Validate object workspace      │
│  - Enforce workspace inheritance │    │  - Allow access to session tree   │
└──────────────────────────────────┘    └──────────────────────────────────┘
```

**Access Patterns:**

| Actor            | Target          | Certificate                     | Authorization                        |
| ---------------- | --------------- | ------------------------------- | ------------------------------------ |
| Client           | Session Manager | CN=alice (long-lived)           | RBAC: role → permissions → workspace |
| Client           | Object Cache    | CN=alice (long-lived)           | RBAC: role → permissions → workspace |
| Instance         | Session Manager | CN=session:abc123 (short-lived) | Cert scope + inherited permissions   |
| Instance         | Object Cache    | CN=session:abc123 (short-lived) | Cert scope + inherited permissions   |
| Executor Manager | Session Manager | CN=flame-executor (internal)    | RBAC: root role, wildcard            |

### 3.2 Components

**1. FlameRbac (`common/src/rbac.rs`)**

RBAC configuration and permission evaluation:

```rust
/// Permission string format: "resource:action"
/// Examples: "session:create", "application:*"
pub type Permission = String;

/// Role definition (stored in database, managed via Admin API)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Role {
    pub name: String,
    pub description: String,
    pub permissions: Vec<Permission>,
    pub workspaces: Vec<String>,
}

/// Pre-defined workspace constants
pub const WORKSPACE_DEFAULT: &str = "default";
pub const WORKSPACE_SYSTEM: &str = "system";

impl Role {
    pub fn has_permission(&self, required: &str) -> bool {
        let (req_resource, req_action) = required.split_once(':').unwrap_or((required, "*"));
        
        for perm in &self.permissions {
            let (perm_resource, perm_action) = perm.split_once(':').unwrap_or((perm, "*"));
            
            let resource_match = perm_resource == "*" || perm_resource == req_resource;
            let action_match = perm_action == "*" || perm_action == req_action;
            
            if resource_match && action_match {
                return true;
            }
        }
        
        false
    }
    
    pub fn has_workspace(&self, workspace: &str) -> bool {
        self.workspaces.iter().any(|w| w == "*" || w == workspace)
    }
}
```

**2. Authorization Interceptor (`session_manager/src/apiserver/interceptor.rs`)**

gRPC interceptor combining mTLS identity with RBAC:

```rust
use tonic::{Request, Status};
use tonic::service::Interceptor;

#[derive(Clone)]
pub struct AuthzInterceptor {
    engine: Arc<dyn Engine>,  // Uses existing storage.Engine trait
}

impl AuthzInterceptor {
    /// Extract resource and action from gRPC method name
    fn extract_permission(method: &str) -> (&str, &str) {
        // Method format: /flame.Frontend/CreateSession
        match method {
            m if m.contains("CreateSession") => ("session", "create"),
            m if m.contains("GetSession") => ("session", "read"),
            m if m.contains("CloseSession") => ("session", "delete"),
            m if m.contains("ListSession") => ("session", "read"),
            m if m.contains("RegisterApplication") => ("application", "create"),
            m if m.contains("GetApplication") => ("application", "read"),
            m if m.contains("ListApplication") => ("application", "read"),
            m if m.contains("SubmitTask") => ("task", "create"),
            m if m.contains("CancelTask") => ("task", "cancel"),
            m if m.contains("GetTask") => ("task", "read"),
            _ => ("*", "*"),
        }
    }
}

impl Interceptor for AuthzInterceptor {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        // Extract client identity from TLS connection
        let subject = request.extensions()
            .get::<TlsClientSubject>()
            .map(|s| s.0.clone())
            .unwrap_or_default();
        
        // Extract workspace from metadata
        let workspace = request.metadata()
            .get("x-flame-workspace")
            .and_then(|v| v.to_str().ok())
            .map(String::from)
            .unwrap_or_else(|| WORKSPACE_DEFAULT.to_string());
        
        // Check if this is a session certificate
        if subject.starts_with("session:") {
            return self.authorize_session_cert(&subject, &workspace, &request);
        }
        
        // Extract required permission from method
        let method = request.uri().path();
        let (resource, action) = Self::extract_permission(method);
        
        // Check RBAC permission (includes system workspace access check)
        self.engine.check_permission(&subject, &workspace, resource, action).await
            .map_err(|e| Status::permission_denied(e.to_string()))?;
        
        // Add authorization context for downstream handlers
        request.extensions_mut().insert(AuthzContext {
            subject,
            workspace,
            scope: CredentialScope::User,  // Direct client always has USER scope
        });
        
        Ok(request)
    }
    
    fn authorize_session_cert(
        &self,
        subject: &str,
        workspace: &str,
        request: &Request<()>,
    ) -> Result<Request<()>, Status> {
        // Session certs have format: session:{session_id}
        // Workspace is encoded in certificate SAN
        // TODO: Extract and validate workspace from cert SAN
        
        // Session certs can only access their own workspace
        // (validation happens in certificate parsing)
        
        Ok(request.clone())
    }
}

/// Authorization context passed to handlers
#[derive(Clone, Debug)]
pub struct AuthzContext {
    pub subject: String,              // User CN (for USER scope) or session_id (for SESSION scope)
    pub workspace: String,
    pub scope: CredentialScope,       // USER for direct client, SESSION for session cert
}
```

**3. mTLS Server Configuration (`common/src/ctx.rs`)**

Extended TLS configuration for mTLS:

```rust
#[derive(Debug, Clone)]
pub struct FlameTls {
    pub cert_file: String,
    pub key_file: String,
    pub ca_file: Option<String>,  // When set, enables mTLS (client cert verification)
}

impl FlameTls {
    /// Load server TLS config with optional mTLS
    /// When ca_file is set, client certificate verification is enabled
    pub fn server_tls_config(&self) -> Result<ServerTlsConfig, FlameError> {
        let cert = fs::read_to_string(&self.cert_file)?;
        let key = fs::read_to_string(&self.key_file)?;
        let identity = Identity::from_pem(cert, key);
        
        let mut config = ServerTlsConfig::new().identity(identity);
        
        // Enable mTLS when ca_file is configured
        if let Some(ref ca_file) = self.ca_file {
            let ca = fs::read_to_string(ca_file)?;
            let ca_cert = Certificate::from_pem(ca);
            config = config.client_ca_root(ca_cert);
        }
        
        Ok(config)
    }
}
```

**4. Workspace in Domain Models (`common/src/apis.rs`)**

Add workspace to all domain structs:

```rust
// Client resources - use user-specified workspace or "default"
#[derive(Clone, Debug, Default)]
pub struct Application {
    pub name: String,
    pub workspace: String,  // NEW: user-specified or "default"
    pub version: u32,
    pub state: ApplicationState,
    // ... other fields
}

#[derive(Debug, Default)]
pub struct Session {
    pub id: SessionID,
    pub workspace: String,  // NEW: user-specified or "default"
    pub application: String,
    pub slots: u32,
    // ... other fields
}

#[derive(Clone, Debug)]
pub struct Task {
    pub id: TaskID,
    pub ssn_id: SessionID,
    pub workspace: String,  // NEW: inherited from session
    // ... other fields
}

// Internal components - always use "system" workspace
#[derive(Clone, Debug, Default)]
pub struct Node {
    pub name: String,
    pub workspace: String,  // NEW: always WORKSPACE_SYSTEM
    // ... other fields
}

#[derive(Clone, Debug, Default)]
pub struct Executor {
    pub id: String,
    pub workspace: String,  // NEW: always WORKSPACE_SYSTEM
    // ... other fields
}
```

**5. Client Certificate Configuration (`sdk/rust/src/apis/ctx.rs`)**

Extended client TLS config for mTLS:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FlameClientTls {
    /// Path to CA certificate for server verification
    #[serde(default)]
    pub ca_file: Option<String>,
    /// Path to client certificate for mTLS
    #[serde(default)]
    pub cert_file: Option<String>,
    /// Path to client private key for mTLS
    #[serde(default)]
    pub key_file: Option<String>,
}

impl FlameClientTls {
    pub fn client_tls_config(&self, domain: &str) -> Result<ClientTlsConfig, FlameError> {
        let mut config = ClientTlsConfig::new().domain_name(domain);
        
        // CA for server verification
        if let Some(ref ca_file) = self.ca_file {
            let ca = fs::read_to_string(ca_file)?;
            config = config.ca_certificate(Certificate::from_pem(ca));
        }
        
        // Client certificate for mTLS
        if let (Some(cert_file), Some(key_file)) = (&self.cert_file, &self.key_file) {
            let cert = fs::read_to_string(cert_file)?;
            let key = fs::read_to_string(key_file)?;
            config = config.identity(Identity::from_pem(cert, key));
        }
        
        Ok(config)
    }
}
```

### 3.3 Data Structures

**Proto Changes (`rpc/protos/types.proto`):**

```protobuf
message Metadata {
  string id = 1;
  string name = 2;
  string workspace = 3;  // NEW: Workspace this resource belongs to
}
```

**Cache Key Structure:**

Object cache keys include workspace for isolation:

```
Current:  {session_id}/{object_id}
New:      {workspace}/{session_id}/{object_id}

Example:
  team-a/train-001/model-v1
  team-a/train-001/checkpoint
  team-b/pipeline-001/dataset
  default/legacy-session/data  (default workspace)
```

**Storage Schema:**

RBAC data (users, roles, workspaces) is stored using the existing storage engine (same as sessions, applications, etc.). The `storage::Engine` trait is extended with RBAC methods.

*Engine Trait Extension (`session_manager/src/storage/engine/mod.rs`):*

```rust
pub trait Engine: Send + Sync + 'static {
    // ... existing methods for Application, Session, Task, Node, Executor ...
    
    // ============================================================
    // RBAC: User Management
    // ============================================================
    async fn get_user_by_cn(&self, cn: &str) -> Result<Option<User>, FlameError>;
    async fn get_user_roles(&self, user_name: &str) -> Result<Vec<Role>, FlameError>;
    async fn create_user(&self, user: &User) -> Result<User, FlameError>;
    async fn update_user(
        &self,
        user: &User,
        assign_roles: &[String],
        revoke_roles: &[String],
    ) -> Result<User, FlameError>;
    async fn delete_user(&self, name: &str) -> Result<(), FlameError>;
    async fn find_users(&self, role_filter: Option<&str>) -> Result<Vec<User>, FlameError>;
    
    // ============================================================
    // RBAC: Role Management
    // ============================================================
    async fn get_role(&self, name: &str) -> Result<Option<Role>, FlameError>;
    async fn create_role(&self, role: &Role) -> Result<Role, FlameError>;
    async fn update_role(&self, role: &Role) -> Result<Role, FlameError>;
    async fn delete_role(&self, name: &str) -> Result<(), FlameError>;
    async fn find_roles(&self, workspace_filter: Option<&str>) -> Result<Vec<Role>, FlameError>;
    
    // ============================================================
    // RBAC: Workspace Management
    // ============================================================
    async fn get_workspace(&self, name: &str) -> Result<Option<Workspace>, FlameError>;
    async fn create_workspace(&self, workspace: &Workspace) -> Result<Workspace, FlameError>;
    async fn update_workspace(&self, workspace: &Workspace) -> Result<Workspace, FlameError>;
    async fn delete_workspace(&self, name: &str) -> Result<(), FlameError>;
    async fn find_workspaces(&self) -> Result<Vec<Workspace>, FlameError>;
}
```

*SQLite Storage (`storage: sqlite://${FLAME_HOME}/flame.db`):*

```sql
-- Add workspace column to all tables
ALTER TABLE applications ADD COLUMN workspace TEXT NOT NULL DEFAULT 'default';
ALTER TABLE sessions ADD COLUMN workspace TEXT NOT NULL DEFAULT 'default';
ALTER TABLE executors ADD COLUMN workspace TEXT NOT NULL DEFAULT 'system';

-- RBAC tables
CREATE TABLE users (
    name TEXT PRIMARY KEY,
    display_name TEXT,
    email TEXT,
    certificate_cn TEXT UNIQUE NOT NULL,
    enabled INTEGER DEFAULT 1,
    created_at INTEGER
);

CREATE TABLE roles (
    name TEXT PRIMARY KEY,
    description TEXT,
    permissions TEXT,  -- JSON array: ["session:*", "application:read"]
    workspaces TEXT,   -- JSON array: ["team-a", "team-b"]
    created_at INTEGER
);

CREATE TABLE user_roles (
    user_name TEXT REFERENCES users(name),
    role_name TEXT REFERENCES roles(name),
    PRIMARY KEY (user_name, role_name)
);

-- Create index for workspace filtering
CREATE INDEX idx_applications_workspace ON applications(workspace);
CREATE INDEX idx_sessions_workspace ON sessions(workspace);
CREATE INDEX idx_users_cn ON users(certificate_cn);
```

*Filesystem Storage (`storage: file://${FLAME_HOME}/data`):*

Follows the existing filesystem storage pattern (same as sessions, applications, nodes) using JSON metadata files:

```
${FLAME_HOME}/data/
├── sessions/<session_id>/
│   └── metadata              # Existing session metadata (JSON)
├── applications/<app_name>/
│   └── metadata              # Existing application metadata (JSON)
├── nodes/<node_name>/
│   └── metadata              # Existing node metadata (JSON)
├── users/<user_name>/
│   └── metadata              # User metadata (JSON)
├── roles/<role_name>/
│   └── metadata              # Role metadata (JSON)
└── workspaces/<workspace_name>/
    └── metadata              # Workspace metadata (JSON)
```

User metadata (`${FLAME_HOME}/data/users/alice/metadata`):
```json
{
  "name": "alice",
  "display_name": "Alice Smith",
  "email": "alice@example.com",
  "certificate_cn": "alice",
  "enabled": true,
  "roles": ["developer", "data-scientist"],
  "creation_time": 1711875600
}
```

Role metadata (`${FLAME_HOME}/data/roles/developer/metadata`):
```json
{
  "name": "developer",
  "description": "Developer role",
  "permissions": ["application:*", "session:*"],
  "workspaces": ["team-a", "team-b"],
  "creation_time": 1711875600
}
```

Workspace metadata (`${FLAME_HOME}/data/workspaces/team-a/metadata`):
```json
{
  "name": "team-a",
  "description": "Team A workspace",
  "labels": {"team": "alpha", "env": "development"},
  "creation_time": 1711875600
}
```

### 3.4 System Considerations

**Performance:**

- mTLS adds ~1-2ms per connection (same as TLS)
- Authorization lookup is O(n) where n = number of policies (typically < 100)
- Workspace filtering adds indexed query condition (negligible overhead)

**Scalability:**

- RBAC data stored via existing storage.Engine (same backend as sessions, etc.)
- Authorization lookups use the same caching layer as other storage operations
- Workspace isolation is enforced at application layer, not storage

**Reliability:**

- Invalid client certificates fail fast at connection
- Missing authorization policies result in denied access (fail-closed)
- Default `system` workspace ensures backward compatibility

**Security:**

- Certificate-based identity cannot be forged
- Workspace isolation prevents cross-tenant data access
- Internal components use dedicated certificates with root role
- No anonymous access when authorization is enabled (mTLS required)

**Observability:**

- Log authorization decisions: "Client 'team-a' authorized for workspace 'team-a'"
- Log denied requests: "DENIED: Client 'team-a' requested workspace 'team-b'"
- Metrics: `flame_authz_decisions{result="allowed|denied",workspace="..."}`

### 3.5 Dependencies

No new external dependencies. Uses existing:

- `tonic` with `tls` feature for mTLS
- `rustls` for certificate handling

## 4. Use Cases

### Example 1: Multi-Tenant ML Platform

**Description:** Multiple teams share a Flame cluster with isolated workspaces. Users are granted access to specific workspaces via RBAC.

**Setup:**

1. Initialize cluster and create admin:
```bash
# Install cluster with mTLS enabled
flmadm install --with-mtls
```

2. Create workspaces and roles:
```bash
# Create workspaces
flmadm create --workspace team-a --description "Team A workspace"
flmadm create --workspace team-b --description "Team B workspace"
flmadm create --workspace staging --description "Staging environment"

# Create roles with permissions and workspaces
flmadm create --role team-a-developer \
  --permission "application:*" \
  --permission "session:*" \
  --workspace team-a

flmadm create --role team-b-developer \
  --permission "application:*" \
  --permission "session:*" \
  --workspace team-b

flmadm create --role ci-deployer \
  --permission "application:*" \
  --permission "session:create" \
  --permission "session:read" \
  --workspace team-a \
  --workspace team-b \
  --workspace staging
```

3. Create users and assign roles:
```bash
# Create users (certificates generated to current directory)
flmadm create --user alice --display-name "Alice" --cert-dir .
flmadm create --user bob --display-name "Bob" --cert-dir .
flmadm create --user ci-pipeline --display-name "CI Pipeline" --cert-dir .

# Assign roles
flmadm update --user alice --assign-role team-a-developer
flmadm update --user bob --assign-role team-b-developer
flmadm update --user ci-pipeline --assign-role ci-deployer

# Generated certificates: alice.crt, alice.key, bob.crt, bob.key, ci-pipeline.crt, ci-pipeline.key
```

4. Alice's client config:
```yaml
contexts:
  - name: alice
    cluster:
      endpoint: "https://flame:8080"
      tls:
        ca_file: /etc/flame/certs/ca.crt
        cert_file: ~/.flame/alice.crt
        key_file: ~/.flame/alice.key
    workspace: team-a  # Default workspace for this context
```

**Workflow:**

```bash
# Alice registers an application in team-a workspace
flmctl --context alice register -f ml-training.yaml
# Application created: team-a/ml-training

# Alice creates a session
flmctl --context alice create -a ml-training -s 4
# Session created: team-a/train-001

# Bob (who only has access to team-b) cannot see team-a's resources
flmctl --context bob list -s --workspace team-a
# Error: PERMISSION_DENIED - subject 'bob' not authorized for workspace 'team-a'

# Bob can only access team-b workspace
flmctl --context bob list -s --workspace team-b
# (shows Bob's sessions in team-b)
```

### Example 2: Executor Manager Cache Access

**Description:** Executor Manager accessing Object Cache on behalf of sessions.

**Setup:**

1. Generate internal certificate for Executor Manager:
```bash
openssl req -new -key executor-manager.key -out executor-manager.csr \
  -subj "/CN=flame-executor/O=Flame"
openssl x509 -req -in executor-manager.csr -CA ca.crt -CAkey ca.key \
  -out executor-manager.crt
```

2. Configure Executor Manager:
```yaml
cluster:
  endpoint: "https://flame-session-manager:8080"
  tls:
    ca_file: /etc/flame/certs/ca.crt
    cert_file: /etc/flame/certs/executor-manager.crt
    key_file: /etc/flame/certs/executor-manager.key

cache:
  endpoint: "grpcs://flame-object-cache:9090"
  tls:
    ca_file: /etc/flame/certs/ca.crt
    cert_file: /etc/flame/certs/executor-manager.crt
    key_file: /etc/flame/certs/executor-manager.key
```

**Workflow:**

When an executor runs a task from Team A's session:

1. Executor Manager receives task from Session Manager
2. Task belongs to session `team-a/train-001`
3. Executor Manager accesses cache with:
   - Client cert: `CN=flame-executor`
   - Metadata: `x-flame-workspace: team-a`, `x-flame-session: train-001`
4. Cache validates internal access and allows read/write to `team-a/train-001/*`

### Example 3: Shared Workspace for Common Data

**Description:** Teams share common datasets via a shared workspace.

**Setup:**

```bash
# Create shared workspace
flmadm create --workspace shared --description "Shared datasets"

# Create role for data admin with full access to shared workspace
flmadm create --role data-admin \
  --permission "application:*" \
  --permission "session:*" \
  --workspace shared

# Give alice and bob read access to shared workspace
flmadm create --role shared-reader \
  --permission "application:read" \
  --permission "session:read" \
  --workspace shared

# Assign roles
flmadm update --user alice --assign-role shared-reader
flmadm update --user bob --assign-role shared-reader
flmadm create --user data-admin --cert-dir .
flmadm update --user data-admin --assign-role data-admin
```

**Workflow:**

```bash
# Data admin uploads shared dataset
flmctl --context data-admin register -f data-loader.yaml --workspace shared
flmctl --context data-admin create -a data-loader -s 1 --workspace shared

# Alice can read from shared workspace (she has access via shared-reader role)
flmctl --context alice list -s --workspace shared
# Shows: shared/load-dataset

# Alice can also use shared applications in her own workspace
flmctl --context alice create -a shared/data-loader -s 1 --workspace team-a
```

### Example 4: Migration from Single-Tenant

**Description:** Gradual migration from single-tenant to multi-tenant setup.

**Phase 1: Enable TLS (no mTLS yet)**
```yaml
cluster:
  tls:
    cert_file: /etc/flame/certs/server.crt
    key_file: /etc/flame/certs/server.key
    # No ca_file = no mTLS, server-only TLS
```

**Phase 2: Enable mTLS and RBAC**
```yaml
cluster:
  tls:
    cert_file: /etc/flame/certs/server.crt
    key_file: /etc/flame/certs/server.key
    ca_file: /etc/flame/certs/ca.crt    # Enables mTLS and RBAC
    ca_key: /etc/flame/certs/ca.key     # For signing session certificates
    cert_validity: 24h
```

```bash
# Install cluster with mTLS first (if not done)
flmadm install --with-mtls

# Create roles with access to default workspace for legacy resources
flmadm create --role legacy-user \
  --permission "application:*" \
  --permission "session:*" \
  --workspace default

# Assign to existing users
flmadm create --user alice --cert-dir .
flmadm update --user alice --assign-role legacy-user
# Note: Legacy resources without workspace go to "default" workspace
```

## 5. References

**Related Documents:**

- RFE234: Enable TLS for All Components (foundation for this design)
- `docs/designs/templates.md`: Design document template

**External References:**

- [tonic mTLS documentation](https://docs.rs/tonic/latest/tonic/transport/index.html#mutual-tls)
- [X.509 Certificate Subject](https://www.rfc-editor.org/rfc/rfc5280#section-4.1.2.6)
- [gRPC Authentication](https://grpc.io/docs/guides/auth/)
- [Kubernetes Namespace Concept](https://kubernetes.io/docs/concepts/overview/working-with-objects/namespaces/)

**Implementation References:**

- Proto definitions: `rpc/protos/types.proto`, `rpc/protos/frontend.proto`
- TLS config: `common/src/ctx.rs`
- Session Manager: `session_manager/src/apiserver/mod.rs`
- Object Cache: `object_cache/src/cache.rs`
- SDK client: `sdk/rust/src/client/mod.rs`

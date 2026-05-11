# RFE352: Enhance OpenSession RPC to Support Session Creation

## 1. Motivation

**Background:**

Flame currently provides separate `open_session` and `create_session` RPCs, requiring users to explicitly manage session lifecycle:

1. **Double-Check Pattern**: Users must first check if a session exists before deciding to create or open it, leading to boilerplate code and potential race conditions.

2. **Inconvenient Workflow**: For common use cases where users simply want to "get or create" a session with a specific ID, the current API requires multiple round trips and conditional logic.

3. **SDK Complexity**: The Python SDK must maintain separate code paths for session creation vs. opening, complicating the implementation for features like `SessionContext` (RFE350).

**Target:**

This design enhances `OpenSession` RPC to optionally create a session if it doesn't exist and a `SessionSpec` is provided. This enables:

1. **Atomic Get-or-Create**: Single RPC call that either opens an existing session or creates a new one.

2. **Spec Validation**: When a session exists and a spec is provided, validate that the specs match to prevent configuration drift.

3. **Backward Compatibility**: Existing behavior (open existing session by ID only) remains unchanged when no spec is provided.

## 2. Function Specification

### Configuration

No new configuration options required. The enhancement uses existing `SessionSpec` structure.

### API

#### gRPC/Protobuf API

**Enhanced OpenSessionRequest:**

```protobuf
message OpenSessionRequest {
  string session_id = 1;
  optional SessionSpec session = 2;  // NEW: Optional session spec for creation
}
```

#### Rust API

**Controller Interface:**

```rust
impl Controller {
    /// Open an existing session or create a new one if spec is provided.
    ///
    /// # Arguments
    /// * `id` - The session ID to open or create
    /// * `spec` - Optional session specification for creation
    ///
    /// # Returns
    /// * `Ok(Session)` - The opened or newly created session
    /// * `Err(FlameError::NotFound)` - Session not found and no spec provided
    /// * `Err(FlameError::InvalidState)` - Session exists but is not open
    /// * `Err(FlameError::InvalidConfig)` - Session exists but spec doesn't match
    pub async fn open_session(
        &self,
        id: SessionID,
        spec: Option<SessionAttributes>,
    ) -> Result<Session, FlameError>;
}
```

**Storage Interface:**

```rust
impl Storage {
    /// Open an existing session or create a new one if spec is provided.
    ///
    /// This method implements atomic get-or-create semantics:
    /// - If session exists and spec provided: validate spec matches, return session
    /// - If session exists and no spec: return session (current behavior)
    /// - If session doesn't exist and spec provided: create session with spec
    /// - If session doesn't exist and no spec: return NotFound error
    pub async fn open_session(
        &self,
        id: SessionID,
        spec: Option<SessionAttributes>,
    ) -> Result<Session, FlameError>;
}
```

#### Rust SDK API

**Connection Method:**

```rust
impl Connection {
    /// Open an existing session or create a new one if spec is provided.
    ///
    /// # Arguments
    /// * `id` - The session ID to open or create
    /// * `spec` - Optional session attributes for creation/validation
    ///
    /// # Returns
    /// * `Ok(Session)` - The opened or newly created session
    /// * `Err(FlameError::NotFound)` - Session not found and no spec provided
    /// * `Err(FlameError::InvalidState)` - Session exists but is not open
    /// * `Err(FlameError::InvalidConfig)` - Session exists but spec doesn't match
    ///
    /// # Example
    /// ```rust
    /// // Open an existing session. Returns an error if not found.
    /// let session = conn.open_session("my-session-id", None).await?;
    ///
    /// // Open or create session with spec
    /// let session = conn.open_session(
    ///     "my-session-id",
    ///     Some(SessionAttributes {
    ///         id: "my-session-id".to_string(),
    ///         application: "my-app".to_string(),
    ///         common_data: None,
    ///         min_instances: 0,
    ///         max_instances: Some(10),
    ///         resreq: None, // server will apply cluster.resreq
    ///     })
    /// ).await?;
    /// ```
    pub async fn open_session(
        &self,
        id: &SessionID,
        spec: Option<&SessionAttributes>,
    ) -> Result<Session, FlameError>;
}
```

#### Python API

**Module-level Function:**

```python
def open_session(
    session_id: SessionID,
    spec: Optional[SessionAttributes] = None,
) -> Session:
    """Open an existing session or create a new one if spec is provided.

    Args:
        session_id: The session ID to open or create.
        spec: Optional session specification. If provided and session doesn't
              exist, a new session will be created with this spec. If session
              exists, the spec will be validated against the existing session.

    Returns:
        The opened or newly created Session object.

    Raises:
        FlameError(NOT_FOUND): If session doesn't exist and no spec provided.
        FlameError(INVALID_STATE): If session exists but is not in Open state.
        FlameError(INVALID_ARGUMENT): If session exists but spec doesn't match.

    Example:
        # Open existing session (current behavior)
        session = open_session("my-session-id")

        # Open or create session with spec
        session = open_session(
            "my-session-id",
            spec=SessionAttributes(
                application="my-app",
                resreq=ResourceRequirement(cpu=1, memory="1g"),
                min_instances=0,
                max_instances=10,
            )
        )
    """
```

**Connection Class Method:**

```python
class Connection:
    def open_session(
        self,
        session_id: SessionID,
        spec: Optional[SessionAttributes] = None,
    ) -> Session:
        """Open an existing session or create a new one if spec is provided.

        Args:
            session_id: The session ID to open or create.
            spec: Optional session specification for creation/validation.

        Returns:
            The opened or newly created Session object.
        """
```

**Behavior Matrix:**

| Session Exists | Spec Provided | Behavior |
|----------------|---------------|----------|
| Yes | No | Return existing session (current behavior) |
| Yes | Yes | Compare specs; return session if match, error if mismatch |
| No | No | Return error: "session not found" (current behavior) |
| No | Yes | Create new session with provided spec |

**Spec Comparison Rules:**

When comparing specs, the following fields are checked for equality:
- `application` (required match)
- `resreq` (required match)
- `min_instances` (required match)
- `max_instances` (required match)

The `common_data` field is NOT compared as it may contain runtime-specific data.

**Error Codes:**

| Scenario | Error |
|----------|-------|
| Session not found and no spec | `NOT_FOUND`: "session <id> not found" |
| Session exists but closed | `INVALID_STATE`: "session <id> is not open" |
| Session exists but spec mismatch | `INVALID_ARGUMENT`: "session <id> spec mismatch: <field> differs" |

### CLI

No CLI changes required. The `flmctl` commands continue to work with the existing session management workflow.

### Other Interfaces

No other interface changes required. The enhancement is fully contained within the gRPC, Rust, and Python APIs defined above.

### Scope

**In Scope:**
- Enhance `OpenSessionRequest` protobuf to include optional `SessionSpec`
- Update session manager to handle get-or-create logic
- Update storage layer with spec comparison logic
- Update Python SDK `open_session` function signature
- Add validation for spec comparison

**Out of Scope:**
- Updating an existing session's spec (requires separate RPC)
- Comparing `common_data` field (runtime-specific)
- Automatic session migration/upgrade between specs

**Limitations:**
- Spec comparison is strict equality (no partial updates)
- Cannot change a session's spec after creation
- `common_data` is not compared to allow runtime flexibility

### Feature Interaction

**Related Features:**
- **RFE350 SessionContext**: Enables cleaner integration by allowing `Runner.service()` to use a single `open_session` call with custom session_id and spec
- **Session Manager Storage**: Core implementation location for get-or-create logic

**Updates Required:**

**Protobuf:**

1. **rpc/protos/frontend.proto**:
   - Add `optional SessionSpec session = 2` to `OpenSessionRequest`

**Rust (Session Manager):**

2. **session_manager/src/apiserver/frontend.rs**:
   - Update `open_session` handler to extract optional `SessionSpec`
   - Convert to `SessionAttributes` and forward to controller

3. **session_manager/src/controller/mod.rs**:
   - Change signature: `open_session(&self, id: SessionID, spec: Option<SessionAttributes>)`
   - Forward spec to storage layer

4. **session_manager/src/storage/mod.rs**:
   - Update `open_session(id, spec)` method signature to accept optional spec
   - Forward to engine layer for atomic get-or-create operation
   - Update in-memory cache with result

5. **session_manager/src/storage/engine/mod.rs** (Engine trait):
   - Add `open_session(id, spec)` method to `Engine` trait

6. **session_manager/src/storage/engine/sqlite.rs** (SqliteEngine):
   - Implement `open_session` with atomic get-or-create in single transaction
   - Add `compare_specs()` helper for spec validation

**Rust (SDK):**

7. **sdk/rust/src/client/mod.rs**:
   - Add `open_session(&self, id: &SessionID, spec: Option<&SessionAttributes>)` method to `Connection`
   - Build `OpenSessionRequest` with optional `SessionSpec`
   - Handle response and return `Session`

**Python (SDK):**

8. **sdk/python/src/flamepy/core/client.py** - Module function:
   - Update `open_session(session_id, spec=None)` signature
   - Forward spec parameter to `Connection.open_session()`

9. **sdk/python/src/flamepy/core/client.py** - Connection class:
   - Update `Connection.open_session(session_id, spec=None)` method
   - Build `SessionSpec` protobuf when spec is provided
   - Include spec in `OpenSessionRequest`

**Auto-generated:**

10. **sdk/python/src/flamepy/proto/** (auto-generated):
   - Regenerate protobuf stubs after proto changes
   - Run: `make proto` or equivalent build command

11. **sdk/rust/** (auto-generated via build.rs):
   - Protobuf stubs regenerated automatically on build

**Integration Points:**
- **Python SDK -> gRPC -> Session Manager**: Enhanced request flow with optional spec
- **Rust SDK -> gRPC -> Session Manager**: Enhanced request flow with optional spec
- **Controller -> Storage**: Controller forwards optional spec to storage's `open_session`
- **Storage Layer**: `open_session` handles all get-or-create logic internally

**Compatibility:**
- Fully backward compatible - existing clients sending `OpenSessionRequest` without spec continue to work
- Protobuf `optional` field ensures wire compatibility

**Breaking Changes:**
- None

## 3. Implementation Detail

### Architecture

```
+------------------------------------------------------------------+
|                     Client Applications                           |
+------------------------------------------------------------------+
          |                                       |
          v                                       v
+------------------------+            +------------------------+
|     Python SDK         |            |      Rust SDK          |
|  (flamepy.core.client) |            | (sdk/rust/src/client)  |
|  open_session(id,spec) |            | open_session(id,spec)  |
+------------------------+            +------------------------+
          |                                       |
          +-------------------+-------------------+
                              |
                              v OpenSessionRequest(session_id, session?)
+------------------------------------------------------------------+
|                      gRPC Frontend (Rust)                         |
|  session_manager/src/apiserver/frontend.rs                        |
|  - Parse OpenSessionRequest with optional SessionSpec             |
|  - Convert protobuf types to internal types                       |
|  - Forward to controller                                          |
+-----------------------------+------------------------------------+
                              |
                              v
+------------------------------------------------------------------+
|                    Controller (Rust)                              |
|  session_manager/src/controller/mod.rs                            |
|  - Forward to storage.open_session(id, spec)                      |
+-----------------------------+------------------------------------+
                              |
                              v
+------------------------------------------------------------------+
|                     Storage (Rust)                                |
|  session_manager/src/storage/mod.rs                               |
|  - open_session(id, spec): forward to engine, update cache        |
+-----------------------------+------------------------------------+
                              |
                              v
+------------------------------------------------------------------+
|                     Engine (Rust)                                 |
|  session_manager/src/storage/engine/sqlite.rs                     |
|  - open_session(id, spec): atomic get-or-create in transaction    |
|  - compare_specs(): validate existing session against new spec    |
+------------------------------------------------------------------+
```

### Components

**1. Protobuf Definition**
- **Location**: `rpc/protos/frontend.proto`
- **Changes**:
  - Add `optional SessionSpec session = 2` to `OpenSessionRequest`
- **Impact**: Regenerate all language bindings (Rust, Python, Go)

**2. Frontend API (Rust)**
- **Location**: `session_manager/src/apiserver/frontend.rs`
- **Changes**:
  - Update `open_session` handler to extract optional `SessionSpec`
  - Convert `SessionSpec` to `SessionAttributes` when present
  - Forward both `session_id` and optional spec to controller
- **Current Code** (lines 305-324):
  ```rust
  async fn open_session(&self, req: Request<OpenSessionRequest>) -> Result<Response<rpc::Session>, Status> {
      // Currently only extracts session_id
      // Will be updated to also extract optional session spec
  }
  ```

**3. Controller (Rust)**
- **Location**: `session_manager/src/controller/mod.rs`
- **Changes**:
  - Update `open_session` method signature: `open_session(&self, id: SessionID, spec: Option<SessionAttributes>)`
  - Forward spec to storage layer
- **Current Code** (lines 66-76):
  ```rust
  pub async fn open_session(&self, id: SessionID) -> Result<Session, FlameError> {
      // Currently only gets existing session
      // Will be updated to accept optional spec and forward to storage
  }
  ```

**4. Storage (Rust)**
- **Location**: `session_manager/src/storage/mod.rs`
- **Changes**:
  - Update `open_session(&self, id: SessionID, spec: Option<SessionAttributes>)` method signature
  - Forward to engine layer for atomic operation
  - Update in-memory session cache with result
- **New Method**:
  ```rust
  pub async fn open_session(
      &self,
      id: SessionID,
      spec: Option<SessionAttributes>,
  ) -> Result<Session, FlameError> {
      // Delegate to engine for atomic get-or-create
      let ssn = self.engine.open_session(id.clone(), spec).await?;

      // Update in-memory cache
      let mut ssn_map = lock_ptr!(self.sessions)?;
      ssn_map.insert(ssn.id.clone(), SessionPtr::new(ssn.clone().into()));

      Ok(ssn)
  }
  ```

**5. Engine Trait (Rust)**
- **Location**: `session_manager/src/storage/engine/mod.rs`
- **Changes**:
  - Add `open_session` method to `Engine` trait
- **New Trait Method**:
  ```rust
  #[async_trait]
  pub trait Engine: Send + Sync + 'static {
      // ... existing methods ...

      /// Open an existing session or create a new one if spec is provided.
      /// Implements atomic get-or-create semantics within a single transaction.
      async fn open_session(
          &self,
          id: SessionID,
          spec: Option<SessionAttributes>,
      ) -> Result<Session, FlameError>;
  }
  ```

**6. SqliteEngine (Rust)**
- **Location**: `session_manager/src/storage/engine/sqlite.rs`
- **Changes**:
  - Extract `_get_session(tx, id)` helper for reuse within transactions
  - Extract `_create_session(tx, attr)` helper for reuse within transactions
  - Refactor existing `get_session` and `create_session` to use helpers
  - Implement `open_session` using the helpers within single transaction
  - Add `compare_specs()` helper for spec validation
- **New Helper Functions**:
  ```rust
  impl SqliteEngine {
      /// Internal helper to get session within an existing transaction
      async fn _get_session(
          tx: &mut SqliteConnection,
          id: SessionID,
      ) -> Result<Option<Session>, FlameError> {
          let sql = "SELECT * FROM sessions WHERE id=?";
          let ssn: Option<SessionDao> = sqlx::query_as(sql)
              .bind(id)
              .fetch_optional(&mut *tx)
              .await
              .map_err(|e| FlameError::Storage(e.to_string()))?;

          match ssn {
              Some(dao) => Ok(Some(dao.try_into()?)),
              None => Ok(None),
          }
      }

      /// Internal helper to create session within an existing transaction
      async fn _create_session(
          tx: &mut SqliteConnection,
          attr: SessionAttributes,
      ) -> Result<Session, FlameError> {
          let common_data: Option<Vec<u8>> = attr.common_data.map(Bytes::into);
          let sql = r#"INSERT INTO sessions (id, application, resreq_cpu, resreq_memory, resreq_gpu, common_data, creation_time, state, min_instances, max_instances)
              VALUES (?, (SELECT name FROM applications WHERE name=? AND state=?), ?, ?, ?, ?, ?, ?, ?, ?)
              RETURNING *"#;
          let resreq = attr.resreq.clone().unwrap_or_default();
          let ssn: SessionDao = sqlx::query_as(sql)
              .bind(attr.id.clone())
              .bind(attr.application)
              .bind(ApplicationState::Enabled as i32)
              .bind(resreq.cpu as i64)
              .bind(resreq.memory as i64)
              .bind(resreq.gpu as i64)
              .bind(common_data)
              .bind(Utc::now().timestamp())
              .bind(SessionState::Open as i32)
              .bind(attr.min_instances as i64)
              .bind(attr.max_instances.map(|v| v as i64))
              .fetch_one(&mut *tx)
              .await
              .map_err(|e| FlameError::Storage(e.to_string()))?;

          ssn.try_into()
      }

      /// Compare session specs for validation
      fn compare_specs(session: &Session, attr: &SessionAttributes) -> Result<(), FlameError> {
          if session.application != attr.application {
              return Err(FlameError::InvalidConfig(format!(
                  "session <{}> spec mismatch: application differs (expected '{}', got '{}')",
                  session.id, session.application, attr.application
              )));
          }
          if session.resreq != attr.resreq {
              return Err(FlameError::InvalidConfig(format!(
                  "session <{}> spec mismatch: resreq differs (expected {:?}, got {:?})",
                  session.id, session.resreq, attr.resreq
              )));
          }
          if session.min_instances != attr.min_instances {
              return Err(FlameError::InvalidConfig(format!(
                  "session <{}> spec mismatch: min_instances differs (expected {}, got {})",
                  session.id, session.min_instances, attr.min_instances
              )));
          }
          if session.max_instances != attr.max_instances {
              return Err(FlameError::InvalidConfig(format!(
                  "session <{}> spec mismatch: max_instances differs (expected {:?}, got {:?})",
                  session.id, session.max_instances, attr.max_instances
              )));
          }
          Ok(())
      }
  }
  ```
- **Refactored Existing Methods**:
  ```rust
  #[async_trait]
  impl Engine for SqliteEngine {
      async fn get_session(&self, id: SessionID) -> Result<Session, FlameError> {
          let mut tx = self.pool.begin().await
              .map_err(|e| FlameError::Storage(e.to_string()))?;

          let ssn = Self::_get_session(&mut tx, id.clone()).await?
              .ok_or_else(|| FlameError::NotFound(format!("session <{id}> not found")))?;

          tx.commit().await
              .map_err(|e| FlameError::Storage(e.to_string()))?;

          Ok(ssn)
      }

      async fn create_session(&self, attr: SessionAttributes) -> Result<Session, FlameError> {
          let mut tx = self.pool.begin().await
              .map_err(|e| FlameError::Storage(e.to_string()))?;

          let ssn = Self::_create_session(&mut tx, attr).await?;

          tx.commit().await
              .map_err(|e| FlameError::Storage(e.to_string()))?;

          Ok(ssn)
      }
  }
  ```
- **New open_session Implementation**:
  ```rust
  async fn open_session(
      &self,
      id: SessionID,
      spec: Option<SessionAttributes>,
  ) -> Result<Session, FlameError> {
      let mut tx = self.pool.begin().await
          .map_err(|e| FlameError::Storage(e.to_string()))?;

      let ssn = match Self::_get_session(&mut tx, id.clone()).await? {
          Some(session) => {
              // Session exists - validate state
              if session.status.state != SessionState::Open {
                  return Err(FlameError::InvalidState(
                      format!("session <{id}> is not open")
                  ));
              }
              // If spec provided, validate it matches
              if let Some(ref attr) = spec {
                  Self::compare_specs(&session, attr)?;
              }
              session
          }
          None => {
              // Session doesn't exist
              match spec {
                  Some(attr) => Self::_create_session(&mut tx, attr).await?,
                  None => return Err(FlameError::NotFound(
                      format!("session <{id}> not found")
                  )),
              }
          }
      };

      tx.commit().await
          .map_err(|e| FlameError::Storage(e.to_string()))?;

      Ok(ssn)
  }
  ```

**7. Rust SDK - Connection**
- **Location**: `sdk/rust/src/client/mod.rs`
- **Changes**:
  - Add new `open_session()` method to `Connection` impl
  - Import `OpenSessionRequest` from rpc module
  - Build request with optional `SessionSpec`
- **New Code**:
  ```rust
  impl Connection {
      pub async fn open_session(
          &self,
          id: &SessionID,
          spec: Option<&SessionAttributes>,
      ) -> Result<Session, FlameError> {
          let session_spec = spec.map(|attrs| SessionSpec {
              application: attrs.application.clone(),
              common_data: attrs.common_data.clone().map(CommonData::into),
              min_instances: attrs.min_instances,
              max_instances: attrs.max_instances,
              resreq: attrs.resreq.clone().map(rpc::ResourceRequirement::from),
          });

          let open_ssn_req = OpenSessionRequest {
              session_id: id.clone(),
              session: session_spec,
          };

          let mut client = FlameClient::new(self.channel.clone());
          let ssn = client.open_session(open_ssn_req).await?;
          let ssn = ssn.into_inner();

          let mut ssn = Session::from(&ssn);
          ssn.client = Some(client);

          Ok(ssn)
      }
  }
  ```

**8. Python SDK - Module Functions**
- **Location**: `sdk/python/src/flamepy/core/client.py`
- **Changes**:
  - Update `open_session()` function signature to accept optional `spec` parameter
  - Forward spec to `Connection.open_session()` when provided
- **Current Code** (lines 80-82):
  ```python
  def open_session(session_id: SessionID) -> "Session":
      conn = ConnectionInstance.instance()
      return conn.open_session(session_id)
  ```
- **New Code**:
  ```python
  def open_session(session_id: SessionID, spec: Optional[SessionAttributes] = None) -> "Session":
      conn = ConnectionInstance.instance()
      return conn.open_session(session_id, spec)
  ```

**9. Python SDK - Connection Class**
- **Location**: `sdk/python/src/flamepy/core/client.py`
- **Changes**:
  - Update `Connection.open_session()` method signature
  - Build `SessionSpec` protobuf when spec is provided
  - Include spec in `OpenSessionRequest`
- **Current Code** (lines 403-428):
  ```python
  def open_session(self, session_id: SessionID) -> "Session":
      request = OpenSessionRequest(session_id=session_id)
      # ...
  ```
- **New Code**:
  ```python
  def open_session(self, session_id: SessionID, spec: Optional[SessionAttributes] = None) -> "Session":
      session_spec = None
      if spec is not None:
          session_spec = SessionSpec(
              application=spec.application,
              common_data=spec.common_data,
              min_instances=spec.min_instances,
              max_instances=spec.max_instances,
              resreq=spec.resreq.to_pb() if spec.resreq else None,
          )
      request = OpenSessionRequest(session_id=session_id, session=session_spec)
      # ...
  ```

**10. Generated Protobuf Stubs**
- **Location**: `sdk/python/src/flamepy/proto/` (auto-generated)
- **Changes**: Regenerate after proto changes using `make proto` or equivalent

### Data Structures

**Enhanced OpenSessionRequest (protobuf):**

```protobuf
message OpenSessionRequest {
  string session_id = 1;
  optional SessionSpec session = 2;
}
```

**SessionSpec (existing, unchanged):**

```protobuf
message SessionSpec {
  string application = 2;
  // Field number 3 (slots) reserved — removed in the slots-cleanup refactor; do not reuse.
  reserved 3;
  reserved "slots";
  optional bytes common_data = 4;
  uint32 min_instances = 5;
  optional uint32 max_instances = 6;
  uint32 batch_size = 7;
  uint32 priority = 8;
  optional ResourceRequirement resreq = 9;
}
```

### Algorithms

**Algorithm 1: Helper Functions (SqliteEngine)**

Extract reusable helpers that operate within an existing transaction:

```rust
impl SqliteEngine {
    /// Get session within existing transaction (returns None if not found)
    async fn _get_session(
        tx: &mut SqliteConnection,
        id: SessionID,
    ) -> Result<Option<Session>, FlameError> {
        let sql = "SELECT * FROM sessions WHERE id=?";
        let ssn: Option<SessionDao> = sqlx::query_as(sql)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        match ssn {
            Some(dao) => Ok(Some(dao.try_into()?)),
            None => Ok(None),
        }
    }

    /// Create session within existing transaction
    async fn _create_session(
        tx: &mut SqliteConnection,
        attr: SessionAttributes,
    ) -> Result<Session, FlameError> {
        let common_data: Option<Vec<u8>> = attr.common_data.map(Bytes::into);
        let sql = r#"INSERT INTO sessions (...) VALUES (...) RETURNING *"#;
        let ssn: SessionDao = sqlx::query_as(sql)
            .bind(...)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;
        ssn.try_into()
    }

    /// Compare specs - returns error if mismatch
    fn compare_specs(session: &Session, attr: &SessionAttributes) -> Result<(), FlameError> {
        // Compare application, resreq, min_instances, max_instances
        // Return FlameError::InvalidConfig on mismatch
    }
}
```

**Algorithm 2: open_session (Engine Layer - SqliteEngine)**

Uses helper functions for clean, atomic get-or-create:

```rust
async fn open_session(
    &self,
    id: SessionID,
    spec: Option<SessionAttributes>,
) -> Result<Session, FlameError> {
    // Begin transaction for atomicity
    let mut tx = self.pool.begin().await?;

    let ssn = match Self::_get_session(&mut tx, id.clone()).await? {
        Some(session) => {
            // Session exists - validate state
            if session.status.state != SessionState::Open {
                return Err(FlameError::InvalidState(
                    format!("session <{id}> is not open")
                ));
            }
            // If spec provided, validate it matches
            if let Some(ref attr) = spec {
                Self::compare_specs(&session, attr)?;
            }
            session
        }
        None => {
            // Session doesn't exist
            match spec {
                Some(attr) => Self::_create_session(&mut tx, attr).await?,
                None => return Err(FlameError::NotFound(
                    format!("session <{id}> not found")
                )),
            }
        }
    };

    // Commit transaction
    tx.commit().await?;

    Ok(ssn)
}
```

**Algorithm 3: Refactored get_session and create_session**

Existing methods refactored to use helpers:

```rust
async fn get_session(&self, id: SessionID) -> Result<Session, FlameError> {
    let mut tx = self.pool.begin().await?;
    let ssn = Self::_get_session(&mut tx, id.clone()).await?
        .ok_or_else(|| FlameError::NotFound(format!("session <{id}> not found")))?;
    tx.commit().await?;
    Ok(ssn)
}

async fn create_session(&self, attr: SessionAttributes) -> Result<Session, FlameError> {
    let mut tx = self.pool.begin().await?;
    let ssn = Self::_create_session(&mut tx, attr).await?;
    tx.commit().await?;
    Ok(ssn)
}
```

**Algorithm 4: open_session (Storage Layer)**

Delegates to engine and updates in-memory cache:

```rust
pub async fn open_session(
    &self,
    id: SessionID,
    spec: Option<SessionAttributes>,
) -> Result<Session, FlameError> {
    trace_fn!("Storage::open_session");

    // Delegate to engine for atomic operation
    let ssn = self.engine.open_session(id.clone(), spec).await?;

    // Update in-memory cache
    let mut ssn_map = lock_ptr!(self.sessions)?;
    ssn_map.insert(ssn.id.clone(), SessionPtr::new(ssn.clone().into()));

    Ok(ssn)
}
```

**Algorithm 5: open_session (Controller Layer - Pass-through)**

```rust
pub async fn open_session(
    &self,
    id: SessionID,
    spec: Option<SessionAttributes>,
) -> Result<Session, FlameError> {
    trace_fn!("Controller::open_session");
    self.storage.open_session(id, spec).await
}
```

### System Considerations

**Performance:**
- Single RPC round-trip for get-or-create (vs. 2 RPCs previously)
- In-memory session lookup before potential creation
- No additional database queries beyond current behavior

**Scalability:**
- No additional state or storage requirements
- Atomic operation prevents race conditions in concurrent access

**Reliability:**
- Spec validation prevents silent configuration drift
- Clear error messages for mismatch scenarios
- Maintains existing session state validation

**Resource Usage:**
- Negligible memory overhead (spec comparison is in-place)
- No additional network or disk I/O

**Security:**
- No new security considerations
- Uses existing session validation and access patterns

**Observability:**
- Logging for open vs. create code paths
- Logging for spec mismatch details

**Operational:**
- No new deployment requirements
- Backward compatible wire protocol

### Dependencies

**External Dependencies:**
- None new

**Internal Dependencies:**
- `common::apis::SessionAttributes` - session attribute types
- `common::apis::SessionState` - session state enum
- `common::FlameError` - error types

## 4. Use Cases

### Basic Use Case: Get or Create Session

**Description:** User wants to ensure a session exists with a specific ID and configuration.

**Step-by-step workflow:**
1. User calls `open_session("my-session-001", spec=SessionSpec(...))`
2. Session manager checks if "my-session-001" exists
3. If not exists: creates session with provided spec
4. If exists: validates spec matches and returns session
5. User receives session handle for task submission

**Code Example:**

```python
from flamepy import open_session, SessionAttributes

# First call - creates the session
session = open_session(
    "my-app-session-001",
    spec=SessionAttributes(
        application="my-app",
        resreq=ResourceRequirement(cpu=1, memory="1g"),
        min_instances=0,
        max_instances=10
    )
)

# Subsequent calls - opens existing session
session2 = open_session(
    "my-app-session-001",
    spec=SessionAttributes(
        application="my-app",
        resreq=ResourceRequirement(cpu=1, memory="1g"),
        min_instances=0,
        max_instances=10
    )
)

# Both return the same session
assert session.id == session2.id
```

**Expected outcome:**
- Session created on first call
- Same session returned on subsequent calls
- No errors if specs match

### Use Case: Spec Mismatch Detection

**Description:** User accidentally tries to open a session with different configuration.

**Step-by-step workflow:**
1. Session "my-session" exists with `resreq=cpu=1,mem=1g`
2. User calls `open_session("my-session", spec=SessionAttributes(resreq=cpu=2,mem=1g))`
3. Session manager detects spec mismatch
4. User receives clear error message

**Code Example:**

```python
from flamepy import open_session, SessionAttributes, ResourceRequirement, FlameError

# Session exists with resreq=cpu=1,mem=1g
try:
    session = open_session(
        "existing-session",
        spec=SessionAttributes(
            application="my-app",
            resreq=ResourceRequirement(cpu=2, memory="1g")  # Mismatch!
        )
    )
except FlameError as e:
    print(f"Error: {e.message}")
    # Output: "session <existing-session> spec mismatch: resreq differs (expected ..., got ...)"
```

**Expected outcome:**
- Clear error message identifying the mismatched field
- Session not modified
- User can fix their configuration

### Use Case: Backward Compatible Open

**Description:** Existing code that uses `open_session` without spec continues to work.

**Code Example:**

```python
from flamepy import open_session

# Existing code pattern - no spec provided
session = open_session("known-session-id")

# Works exactly as before - returns existing session or error if not found
```

**Expected outcome:**
- Existing code unchanged
- Same behavior as current implementation

### Use Case: SessionContext Integration (RFE350)

**Description:** `Runner.service()` uses enhanced `open_session` with custom session ID.

**Step-by-step workflow:**
1. User defines service with `SessionContext(session_id="custom-001")`
2. `Runner.service()` calls `open_session` with ID and spec
3. Session is created if needed, or existing session returned
4. No need for separate check/create logic in SDK

**Code Example:**

```python
from flamepy.runner import Runner, SessionContext

class MyService:
    _session_context = SessionContext(
        session_id="my-custom-session-001"
    )
    
    def process(self, data):
        return data * 2

with Runner("my-app") as runner:
    # Internally uses enhanced open_session with spec
    service = runner.service(MyService)
    result = service.process(42)
```

**Expected outcome:**
- Simplified SDK implementation
- Consistent session handling across create/open scenarios
- Custom session IDs work seamlessly

## 5. References

### Related Documents
- [RFE350 - SessionContext for Custom Session IDs](../RFE350-flame-session-context/FS.md)
- [Design Document Template](../templates.md)

### Implementation References

**Protobuf Definitions:**
- `rpc/protos/frontend.proto` - Frontend RPC definitions (OpenSessionRequest)
- `rpc/protos/types.proto` - Session and SessionSpec types

**Rust (Session Manager):**
- `session_manager/src/apiserver/frontend.rs` - Frontend RPC handlers
- `session_manager/src/controller/mod.rs` - Controller implementation
- `session_manager/src/storage/mod.rs` - Storage layer (caching)
- `session_manager/src/storage/engine/mod.rs` - Engine trait definition
- `session_manager/src/storage/engine/sqlite.rs` - SQLite engine implementation

**Rust (SDK):**
- `sdk/rust/src/client/mod.rs` - Rust SDK client (Connection.open_session)
- `sdk/rust/src/apis/mod.rs` - Rust SDK types

**Python (SDK):**
- `sdk/python/src/flamepy/core/client.py` - Python SDK client (open_session, Connection.open_session)
- `sdk/python/src/flamepy/core/types.py` - Python types (SessionAttributes, SessionID)
- `sdk/python/src/flamepy/proto/` - Auto-generated protobuf stubs

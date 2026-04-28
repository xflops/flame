# RFE423: Add App into Cache's Key

## 1. Motivation

**Background:**

Currently, the flame-object-cache stores objects using a two-level key structure: `<session_id>/<object_id>`. This design works well for session-scoped data but has a significant limitation: `flamepy.runner` needs to store objects that persist across sessions within the same application.

The current key structure creates isolation at the session level, meaning:
1. Objects stored by one session cannot be accessed by another session
2. When a session closes, its cached data becomes orphaned or inaccessible to future sessions
3. Applications that need shared state across multiple sessions (e.g., model weights, configuration, shared datasets) must re-upload data for each session

This limitation affects use cases like:
- **Stateful runners**: Where execution context should be shared across sessions
- **Model serving**: Where large model weights should be loaded once and shared
- **Data pipelines**: Where intermediate results need to be accessible across sessions

**Target:**

This design aims to enhance the object cache to support application-level object sharing by:

1. Introducing a three-level key structure: `<app>/<ssn>/<uuid>`
2. Adding `put_object` method to `Runner` class that uses `<app_name>/shared` as key prefix
3. Reserving `shared` and `default` as special session names for objects
4. Updating all cache operations to use new key format (backward compatibility NOT required)

## 2. Function Specification

### Configuration

No configuration changes required. The feature uses existing cache configuration from `flame-cluster.yaml` and `flame.yaml`.

### API

**Python SDK - Runner Module:**

New method added to `Runner` class for cross-session object storage:

```python
class Runner:
    def put_object(self, obj: Any) -> ObjectRef:
        """Put an object into the cache with <app_name>/shared key prefix.
        
        Uses the runner's app name and "shared" session for cross-session access.
        The full key will be: <app_name>/shared/<object_id>
        
        Args:
            obj: The object to cache (will be pickled)
        
        Returns:
            ObjectRef pointing to the cached object
        
        Example:
            rr = Runner("my-app")
            
            # Store shared data accessible across sessions
            ref = rr.put_object(model_weights)
            # Key: my-app/shared/<uuid>
        """
```

**Object Key Structure:**

The object endpoint/key structure changes from:
```
Current:  <session_id>/<object_id>
New:      <app_name>/<session_id>/<object_id>
```

For `Runner.put_object`, the key uses `shared` as session:
```
<app_name>/shared/<uuid>
```

Examples:
- `my-app/shared/550e8400-e29b-41d4-a716-446655440000` - Shared object via `rr.put_object`
- `my-app/session-123/550e8400-e29b-41d4-a716-446655440001` - Session-specific object

**Reserved Session Names:**

| Session Name | Purpose | Status |
|--------------|---------|--------|
| `shared` | Cross-session shared objects (used by `Runner.put_object`) | Active |
| `default` | Reserved for future use | Reserved only |

These reserved names cannot be used as regular session IDs in the `create_session` API.

**Reserved Name Validation Locations:**

Reserved session names (`shared`, `default`) must be validated at multiple points to prevent misuse:

| Component | Location | Validation | Action |
|-----------|----------|------------|--------|
| **Session Manager** | `create_session` API | Reject `shared`/`default` as session_id | Return error: "Reserved session name" |
| **Cache Server** | `ObjectKey::from_path()` | Allow `shared`/`default` in key prefix | No rejection (valid for `Runner.put_object`) |
| **Python SDK** | `RunnerService.__init__` | Ensure generated session_id doesn't conflict | UUID-based, no collision possible |

**Enforcement Logic (Session Manager):**

```rust
// In session manager's create_session handler
const RESERVED_SESSION_NAMES: &[&str] = &["shared", "default"];

fn validate_session_id(session_id: &str) -> Result<(), FlameError> {
    if RESERVED_SESSION_NAMES.contains(&session_id) {
        return Err(FlameError::InvalidConfig(
            format!("'{}' is a reserved session name and cannot be used", session_id)
        ));
    }
    Ok(())
}
```

**Note:** The cache server does NOT block reserved names because `Runner.put_object` legitimately uses `<app>/shared/<uuid>` keys. The restriction applies only to user-facing session creation APIs.

### Cache Server Updates

**Storage Structure:**

```
/var/lib/flame/cache/
└── <app_name>/
    ├── shared/
    │   ├── object1.arrow
    │   └── object2.arrow
    └── <session_id>/
        ├── object3.arrow
        └── object4.arrow
```

**Key Validation:**

The cache server validates keys in the new format:
- Must have format: `<app_name>/<session_id>/<uuid>`
- `app_name`: Non-empty, no path traversal characters
- `session_id`: Non-empty, no path traversal characters
- `uuid`: UUID format generated by server

### Scope

**In Scope:**
- New three-level key format: `<app>/<ssn>/<uuid>`
- `put_object` method on `Runner` class using `<app>/shared/<uuid>` key
- Reserved session names (`shared`, `default`)
- Storage structure update for three-level hierarchy
- Key validation for new format

**Out of Scope:**
- Backward compatibility with existing `<session_id>/<object_id>` format (explicitly not required per issue)
- Access control or permissions for shared objects
- Garbage collection for orphaned shared objects
- Cross-application object sharing

**Limitations:**
- No automatic cleanup of shared objects when application is unregistered
- No versioning for shared objects (uses existing version=0 semantics)
- Shared objects count against the same LRU eviction policy as session objects
- No namespace isolation between applications with similar names

### Feature Interaction

**Related Features:**
- **RFE318 Object Cache**: Base implementation being extended
- **flamepy.runner**: Primary consumer of this feature
- **Session Management**: Session IDs will be validated against reserved names

**Updates Required:**

1. **object_cache/src/cache.rs**:
   - Update key validation to accept three-level format: `<app>/<ssn>/<uuid>`
   - Update `put`, `get`, `update`, `patch`, `delete` to handle new key format
   - Add validation for reserved session names

2. **object_cache/src/storage/disk.rs**:
   - Update path construction for three-level directory structure
   - Modify directory handling for app/session hierarchy

3. **sdk/python/src/flamepy/core/cache.py**:
   - Add `ObjectKey` dataclass with `from_prefix()`, `from_key()`, `for_shared()` factory methods
   - Update `put_object()` to use `ObjectKey.from_prefix()` for validation
   - Update `get_object()`, `update_object()`, `patch_object()` to use `ObjectKey.from_key()` for validation
   - Update docstrings to reflect new key format

4. **sdk/python/src/flamepy/runner/runner.py**:
   - Add `put_object` method to `Runner` class (uses `<app>/shared` prefix)
   - Update `RunnerService.__init__` to use `<app>/<session_id>` format when storing `common_data`:
     ```python
     # Before
     object_ref = put_object(session_id, serialized_ctx)
     
     # After
     object_ref = put_object(f"{app}/{session_id}", serialized_ctx)
     ```

**Integration Points:**
- Cache server handles new three-level keys: `<app>/<ssn>/<uuid>`
- Python SDK constructs keys with app prefix

**Compatibility:**
- Backward compatibility is NOT required (per issue specification)

**Breaking Changes:**
- Key format changes from `<ssn>/<uuid>` to `<app>/<ssn>/<uuid>`
- `flamepy.core.put_object(key_prefix, obj)` - `key_prefix` must now be `<app>/<ssn>` format
- All existing cached objects will be inaccessible
- All callers of `put_object` must update to provide app-prefixed key_prefix

## 3. Implementation Detail

### Architecture

```
┌─────────────────────────┐
│  Runner                 │
│  rr.put_object(obj)     │
└───────────┬─────────────┘
            │ key_prefix = "<app>/shared"
            ▼
┌─────────────────────────┐
│  flamepy.core.cache     │
│  put_object(key_prefix, │
│             obj)        │
└───────────┬─────────────┘
            │ Arrow Flight do_put
            │ FlightDescriptor.path = "<app>/<ssn>"
            ▼
┌─────────────────────────┐
│  flame-object-cache     │
│  (Arrow Flight Server)  │
│  generates <uuid>       │
└───────────┬─────────────┘
            │ key = "<app>/<ssn>/<uuid>"
            ▼
┌─────────────────────────┐
│   Disk Storage          │
│   /{app}/{ssn}/{uuid}.arrow
└─────────────────────────┘
```

### Components

**1. Runner.put_object (Python)**
- **Location**: `sdk/python/src/flamepy/runner/runner.py`
- **Responsibilities**:
  - Accept object to store
  - Use runner's app name and "shared" as session
  - Delegate to `flamepy.core.cache.put_object`

**2. flamepy.core.cache (Python - Updated)**
- **Location**: `sdk/python/src/flamepy/core/cache.py`
- **Responsibilities**:
  - `put_object(key_prefix, obj)` - validates `key_prefix` is `<app>/<ssn>` format via `ObjectKey.from_prefix()`, then uploads
  - `get_object(ref)` - validates `ref.key` is `<app>/<ssn>/<uuid>` format via `ObjectKey.from_key()`, then retrieves
  - `update_object(ref, obj)` - validates `ref.key` format via `ObjectKey.from_key()`, then updates
  - `patch_object(ref, delta)` - validates `ref.key` format via `ObjectKey.from_key()`, then patches
  - Server generates uuid, full key becomes: `<app>/<ssn>/<uuid>`
  - New `ObjectKey` dataclass with factory methods for key parsing and validation

**2b. ObjectKey (Python - New)**
- **Location**: `sdk/python/src/flamepy/core/cache.py` (alongside cache functions)
- **Responsibilities**:
  - Parse and validate key formats
  - Factory methods: `from_prefix()`, `from_key()`, `for_shared()`
  - Conversion methods: `to_prefix()`, `to_key()`, `with_generated_id()`
  - Immutable dataclass with validation in `__post_init__`

**3. ObjectKey (Rust - New)**
- **Location**: `object_cache/src/cache.rs` (alongside ObjectCache)
- **Responsibilities**:
  - Parse and validate key formats
  - Methods: `from_path()`, `to_key()`, `to_prefix()`, `with_generated_id()`
  - `TryFrom<&str>` for full key parsing
  - `From<&ObjectKey>` for string conversion

**4. ObjectCache (Rust - Updated)**
- **Location**: `object_cache/src/cache.rs`
- **Responsibilities**:
  - Use `ObjectKey` for all key operations
  - Store objects with new three-level key format
  - Enforce reserved session name restrictions (via `ObjectKey` validation)

**5. DiskStorage (Rust - Updated)**
- **Location**: `object_cache/src/storage/disk.rs`
- **Responsibilities**:
  - Create three-level directory structure
  - Handle path construction for app/session/object hierarchy
  - Manage delta directories within the new structure

### Data Structures

**Key Format:**
```
Format: <app_name>/<session_id>/<uuid>

Components:
- app_name:    Application identifier (e.g., "my-runner")
- session_id:  Session namespace or reserved name ("shared", "default", or actual session_id)
- uuid:        UUID generated by cache server
```

**ObjectRef (Python - unchanged structure, new key format):**
```python
@dataclass
class ObjectRef:
    endpoint: str    # Cache server endpoint
    key: str        # "<app>/<ssn>/<uuid>"
    version: int    # Always 0 for now
```

**Storage Organization:**
```
{storage_path}/
└── {app_name}/
    └── {session_id}/                      # e.g., "shared" or actual session ID
        ├── {uuid}.arrow                   # base object file
        └── {uuid}.deltas/                 # delta directory for patches
            ├── 0.arrow
            ├── 1.arrow
            └── ...
```

**In-Memory Cache State (ObjectCache):**

The `ObjectCache` struct maintains in-memory state that must be updated for the new key format:

```rust
pub struct ObjectCache {
    objects: HashMap<String, CacheEntry>,  // Key: "<app>/<ssn>/<uuid>"
    // ... other fields
}

pub struct CacheEntry {
    pub object: Object,
    pub metadata: ObjectMetadata,
}

pub struct ObjectMetadata {
    pub key: String,        // Updated format: "<app>/<ssn>/<uuid>"
    pub size: u64,
    pub created_at: u64,
    pub accessed_at: u64,
}
```

Key changes:
- `ObjectCache.objects` HashMap keys change from `<ssn>/<uuid>` to `<app>/<ssn>/<uuid>`
- `ObjectMetadata.key` field stores the full three-part key
- LRU eviction logic continues to work unchanged (uses key as identifier)
- Cache lookup operations use the new three-part key format

**Current vs New Directory Structure:**

| Aspect | Current (`<ssn>/<uuid>`) | New (`<app>/<ssn>/<uuid>`) |
|--------|--------------------------|----------------------------|
| Base object | `{storage}/{ssn}/{uuid}.arrow` | `{storage}/{app}/{ssn}/{uuid}.arrow` |
| Delta dir | `{storage}/{ssn}/{uuid}.deltas/` | `{storage}/{app}/{ssn}/{uuid}.deltas/` |
| Session dir | `{storage}/{ssn}/` | `{storage}/{app}/{ssn}/` |
| App dir | N/A | `{storage}/{app}/` |

### Algorithms

**Key Parsing (Rust - Updated):**

Current implementation validates 2-part keys. New implementation must handle 3-part keys:

```rust
// Current (to be replaced)
fn parse_key(key: &str) -> Result<(String, String), FlameError> {
    let parts: Vec<&str> = key.split('/').collect();
    if parts.len() != 2 {
        return Err(FlameError::InvalidConfig(...));
    }
    Ok((parts[0].to_string(), parts[1].to_string()))  // (session_id, uuid)
}

// New implementation
fn parse_key(key: &str) -> Result<(String, String, String), FlameError> {
    let parts: Vec<&str> = key.split('/').collect();
    if parts.len() != 3 {
        return Err(FlameError::InvalidConfig(
            format!("Invalid key format '{}': expected <app>/<ssn>/<uuid>", key)
        ));
    }
    
    let (app_name, session_id, uuid) = (parts[0], parts[1], parts[2]);
    
    // Validate no path traversal
    for part in &[app_name, session_id, uuid] {
        if part.contains("..") || part.contains('\\') || part.is_empty() {
            return Err(FlameError::InvalidConfig(
                format!("Invalid key component: {}", part)
            ));
        }
    }
    
    Ok((app_name.to_string(), session_id.to_string(), uuid.to_string()))
}
```

**ObjectKey Type (Rust - New):**

Introduce an `ObjectKey` struct with `TryFrom` implementations to handle key parsing idiomatically:

```rust
/// Represents a parsed object key with app, session, and optional object ID.
#[derive(Debug, Clone)]
pub struct ObjectKey {
    pub app_name: String,
    pub session_id: String,
    pub object_id: Option<String>,  // None for put (server generates), Some for get/update
}

impl ObjectKey {
    /// Parse from path string (2-part "<app>/<ssn>" or 3-part "<app>/<ssn>/<uuid>")
    pub fn from_path(path_str: &str) -> Result<Self, FlameError> {
        let parts: Vec<&str> = path_str.split('/').collect();
        
        // Validate components
        for part in &parts {
            if part.is_empty() || part.contains("..") || part.contains('\\') {
                return Err(FlameError::InvalidConfig(
                    format!("Invalid key component: '{}'", part)
                ));
            }
        }
        
        match parts.len() {
            // "<app>/<ssn>" for put operations (server generates uuid)
            2 => Ok(ObjectKey {
                app_name: parts[0].to_string(),
                session_id: parts[1].to_string(),
                object_id: None,
            }),
            // "<app>/<ssn>/<uuid>" for update/get operations
            3 => Ok(ObjectKey {
                app_name: parts[0].to_string(),
                session_id: parts[1].to_string(),
                object_id: Some(parts[2].to_string()),
            }),
            _ => Err(FlameError::InvalidConfig(
                format!("Invalid path '{}': expected '<app>/<ssn>' or '<app>/<ssn>/<uuid>'", path_str)
            )),
        }
    }
    
    /// Construct full key string: "<app>/<ssn>/<uuid>"
    pub fn to_key(&self) -> Option<String> {
        self.object_id.as_ref().map(|oid| format!("{}/{}/{}", self.app_name, self.session_id, oid))
    }
    
    /// Construct key prefix: "<app>/<ssn>"
    pub fn to_prefix(&self) -> String {
        format!("{}/{}", self.app_name, self.session_id)
    }
    
    /// Create ObjectKey with generated uuid
    pub fn with_generated_id(self) -> Self {
        Self {
            object_id: Some(uuid::Uuid::new_v4().to_string()),
            ..self
        }
    }
}

/// Parse from full key string "<app>/<ssn>/<uuid>"
impl TryFrom<&str> for ObjectKey {
    type Error = FlameError;
    
    fn try_from(key: &str) -> Result<Self, Self::Error> {
        let parts: Vec<&str> = key.split('/').collect();
        
        if parts.len() != 3 {
            return Err(FlameError::InvalidConfig(
                format!("Invalid key '{}': expected '<app>/<ssn>/<uuid>'", key)
            ));
        }
        
        for part in &parts {
            if part.is_empty() || part.contains("..") || part.contains('\\') {
                return Err(FlameError::InvalidConfig(
                    format!("Invalid key component: '{}'", part)
                ));
            }
        }
        
        Ok(ObjectKey {
            app_name: parts[0].to_string(),
            session_id: parts[1].to_string(),
            object_id: Some(parts[2].to_string()),
        })
    }
}

/// Convert back to full key string
impl From<&ObjectKey> for String {
    fn from(key: &ObjectKey) -> Self {
        key.to_key().unwrap_or_else(|| key.to_prefix())
    }
}
```

**Usage in Cache Operations:**

```rust
// In do_put(): extract path from descriptor, parse 2-part, generate uuid
let path_str = descriptor.path.first()
    .ok_or_else(|| FlameError::InvalidConfig("Empty flight descriptor path".into()))?;
let object_key = ObjectKey::from_path(path_str)?.with_generated_id();
let key = object_key.to_key().unwrap();

// In do_get(): extract path from descriptor, parse full 3-part key
let path_str = descriptor.path.first()
    .ok_or_else(|| FlameError::InvalidConfig("Empty flight descriptor path".into()))?;
let object_key = ObjectKey::from_path(path_str)?;
let key = object_key.to_key()
    .ok_or_else(|| FlameError::InvalidConfig("Missing object_id for get".into()))?;

// In DiskStorage: parse from stored key string
let object_key = ObjectKey::try_from(key.as_str())?;
let app_session_dir = self.app_session_dir(&object_key.app_name, &object_key.session_id);
```

**Callers to Update:**
- `do_put()` - Use `ObjectKey::from_path(descriptor.path.first()?)?.with_generated_id()`
- `do_get()` - Use `ObjectKey::from_path(descriptor.path.first()?)?`
- `do_action()` for patch - Use `ObjectKey::try_from(key_str)?`
- `DiskStorage::write_object()` - Use `ObjectKey::try_from(key)?`

**DiskStorage Path Functions (Rust - Updated):**

```rust
impl DiskStorage {
    // New: app directory
    fn app_dir(&self, app_name: &str) -> PathBuf {
        self.storage_path.join(app_name)
    }
    
    // Updated: now includes app_name
    fn app_session_dir(&self, app_name: &str, session_id: &str) -> PathBuf {
        self.storage_path.join(app_name).join(session_id)
    }
    
    // Updated: key is now <app>/<ssn>/<uuid>
    fn object_path(&self, key: &str) -> PathBuf {
        self.storage_path.join(format!("{}.arrow", key))
    }
    
    // Updated: delta dir follows same pattern
    fn delta_dir(&self, key: &str) -> PathBuf {
        self.storage_path.join(format!("{}.deltas", key))
    }
}
```

**write_object (Rust - Updated):**

```rust
async fn write_object(&self, key: &str, object: &Object) -> Result<(), FlameError> {
    let parts: Vec<&str> = key.split('/').collect();
    if parts.len() != 3 {
        return Err(FlameError::InvalidConfig(format!(
            "Invalid key format: {}", key
        )));
    }
    let app_name = parts[0].to_string();
    let session_id = parts[1].to_string();

    let app_session_dir = self.app_session_dir(&app_name, &session_id);
    let object_path = self.object_path(key);
    let delta_dir = self.delta_dir(key);
    let object_clone = object.clone();

    tokio::task::spawn_blocking(move || {
        fs::create_dir_all(&app_session_dir)?;  // Creates {storage}/{app}/{ssn}/
        let batch = object_to_batch(&object_clone)?;
        write_batch_to_file(&object_path, &batch)?;
        if delta_dir.exists() {
            fs::remove_dir_all(&delta_dir)?;
        }
        Ok(())
    }).await?
}
```

**load_objects (Rust - Updated):**

```rust
async fn load_objects(&self) -> Result<Vec<(String, Object, u64)>, FlameError> {
    // Now needs to iterate 3 levels: app -> session -> objects
    for app_entry in fs::read_dir(&storage_path)? {
        let app_path = app_entry?.path();
        if !app_path.is_dir() { continue; }
        let app_name = app_path.file_name()...;
        
        for session_entry in fs::read_dir(&app_path)? {
            let session_path = session_entry?.path();
            if !session_path.is_dir() { continue; }
            let session_id = session_path.file_name()...;
            
            for object_entry in fs::read_dir(&session_path)? {
                // ... load object files
                let key = format!("{}/{}/{}", app_name, session_id, uuid);
                // ...
            }
        }
    }
}
```

**delete_objects (Rust - Updated):**

The `delete_objects` function supports two deletion scopes via prefix format:

| Prefix Format | Scope | Use Case | Caller |
|---------------|-------|----------|--------|
| `<app>/<ssn>` | Session | Delete all objects for a specific session within an app | Session close flow |
| `<app>` | Application | Delete all objects for an entire app | App unregistration (future) |

**API Contract:**
- Called by `ObjectCache.delete()` when a session closes
- Session Manager calls with `<app>/<session_id>` format when closing a session
- Application-level deletion (`<app>` only) is reserved for future app lifecycle management
- Three-part keys (`<app>/<ssn>/<uuid>`) are NOT valid for `delete_objects` - use `delete_object` for single object deletion

**In-Memory State Cleanup:**
When `delete_objects` is called, `ObjectCache` must also remove matching entries from the in-memory `objects` HashMap:

```rust
// In ObjectCache
pub async fn delete(&self, prefix: &str) -> Result<(), FlameError> {
    // 1. Delete from disk storage
    self.storage.delete_objects(prefix).await?;
    
    // 2. Remove from in-memory cache
    let mut objects = self.objects.write().await;
    objects.retain(|key, _| !key.starts_with(&format!("{}/", prefix)));
    
    Ok(())
}
```

**Storage Implementation:**

```rust
async fn delete_objects(&self, prefix: &str) -> Result<(), FlameError> {
    let parts: Vec<&str> = prefix.split('/').collect();
    let dir_to_delete = match parts.len() {
        1 => self.app_dir(parts[0]),           // Delete entire app
        2 => self.app_session_dir(parts[0], parts[1]),  // Delete app/session
        _ => return Err(FlameError::InvalidConfig(
            format!("Invalid prefix '{}': expected '<app>' or '<app>/<ssn>'", prefix)
        )),
    };
    
    if dir_to_delete.exists() {
        fs::remove_dir_all(&dir_to_delete)?;
    }
    Ok(())
}
```

**Python SDK ObjectKey Type (New):**

Introduce an `ObjectKey` dataclass with factory methods to handle key parsing idiomatically:

```python
from dataclasses import dataclass
from typing import Optional
import uuid as uuid_lib


@dataclass(frozen=True)
class ObjectKey:
    """Represents a parsed object key with app, session, and optional object ID.
    
    Attributes:
        app_name: Application identifier
        session_id: Session namespace (e.g., "shared" or actual session ID)
        object_id: Object UUID (None for prefix-only, set for full keys)
    """
    app_name: str
    session_id: str
    object_id: Optional[str] = None
    
    def __post_init__(self):
        """Validate components on creation."""
        for name, value in [("app_name", self.app_name), ("session_id", self.session_id)]:
            if not value:
                raise ValueError(f"{name} cannot be empty")
            if ".." in value or "\\" in value:
                raise ValueError(f"{name} contains invalid characters: '{value}'")
        
        if self.object_id is not None:
            if not self.object_id:
                raise ValueError("object_id cannot be empty string")
            if ".." in self.object_id or "\\" in self.object_id:
                raise ValueError(f"object_id contains invalid characters: '{self.object_id}'")
    
    @classmethod
    def from_prefix(cls, prefix: str) -> "ObjectKey":
        """Parse from key prefix '<app>/<ssn>'.
        
        Args:
            prefix: Key prefix string
            
        Returns:
            ObjectKey with object_id=None
            
        Raises:
            ValueError: If prefix is not in valid format
        """
        parts = prefix.split("/")
        if len(parts) != 2:
            raise ValueError(f"Invalid key prefix '{prefix}': expected '<app>/<ssn>' format")
        return cls(app_name=parts[0], session_id=parts[1], object_id=None)
    
    @classmethod
    def from_key(cls, key: str) -> "ObjectKey":
        """Parse from full key '<app>/<ssn>/<uuid>'.
        
        Args:
            key: Full object key string
            
        Returns:
            ObjectKey with all components set
            
        Raises:
            ValueError: If key is not in valid format
        """
        parts = key.split("/")
        if len(parts) != 3:
            raise ValueError(f"Invalid object key '{key}': expected '<app>/<ssn>/<uuid>' format")
        return cls(app_name=parts[0], session_id=parts[1], object_id=parts[2])
    
    @classmethod
    def for_shared(cls, app_name: str) -> "ObjectKey":
        """Create key for shared storage: '<app>/shared'.
        
        Args:
            app_name: Application name
            
        Returns:
            ObjectKey with session_id="shared"
        """
        return cls(app_name=app_name, session_id="shared", object_id=None)
    
    def with_generated_id(self) -> "ObjectKey":
        """Return new ObjectKey with a generated UUID.
        
        Returns:
            New ObjectKey with object_id set to a new UUID
        """
        return ObjectKey(
            app_name=self.app_name,
            session_id=self.session_id,
            object_id=str(uuid_lib.uuid4())
        )
    
    def to_prefix(self) -> str:
        """Return key prefix '<app>/<ssn>'."""
        return f"{self.app_name}/{self.session_id}"
    
    def to_key(self) -> Optional[str]:
        """Return full key '<app>/<ssn>/<uuid>' or None if object_id not set."""
        if self.object_id is None:
            return None
        return f"{self.app_name}/{self.session_id}/{self.object_id}"
    
    def __str__(self) -> str:
        """Return string representation (full key or prefix)."""
        return self.to_key() or self.to_prefix()
```

**Python SDK put_object (Updated):**

```python
def put_object(key_prefix: str, obj: Any) -> ObjectRef:
    """Put object into cache.
    
    Args:
        key_prefix: Key prefix in format "<app_name>/<session_id>"
        obj: Object to cache
    
    Returns:
        ObjectRef with key "<app_name>/<session_id>/<uuid>"
        
    Raises:
        ValueError: If key_prefix is not in valid <app>/<ssn> format
        
    Example:
        # Store object for specific session
        ref = put_object("my-app/session-123", my_data)
        # Key: my-app/session-123/<uuid>
        
        # Store shared object (typically via Runner.put_object)
        ref = put_object("my-app/shared", shared_data)
        # Key: my-app/shared/<uuid>
    """
    # Validate and parse key prefix
    object_key = ObjectKey.from_prefix(key_prefix)
    
    context = FlameContext()
    cache_config = context.cache
    
    batch = _serialize_object(obj)
    client = _get_flight_client(cache_config.endpoint)
    
    # FlightDescriptor path is "<app>/<ssn>"
    upload_descriptor = flight.FlightDescriptor.for_path(object_key.to_prefix())
    return _do_put_remote(client, upload_descriptor, batch)
```

**Python SDK get_object (Updated):**

```python
def get_object(ref: ObjectRef, deserializer: Optional[Deserializer] = None) -> Any:
    """Get an object from the cache.
    
    Args:
        ref: ObjectRef pointing to the cached object
        deserializer: Optional function to combine base and deltas
    
    Returns:
        The deserialized object
        
    Raises:
        ValueError: If ref.key is not in valid <app>/<ssn>/<uuid> format
    """
    # Validate and parse object key
    object_key = ObjectKey.from_key(ref.key)
    
    tls_config = _get_cache_tls_config()
    client = _get_flight_client(ref.endpoint, tls_config)
    ticket = flight.Ticket(ref.key.encode())
    # ... rest of implementation
```

**Python SDK update_object / patch_object (Updated):**

```python
def update_object(ref: ObjectRef, new_obj: Any) -> ObjectRef:
    """Update an object in the cache."""
    # Validate and parse object key
    object_key = ObjectKey.from_key(ref.key)
    # ... rest of implementation


def patch_object(ref: ObjectRef, delta: Any) -> ObjectRef:
    """Append delta data to an existing cached object."""
    # Validate and parse object key
    object_key = ObjectKey.from_key(ref.key)
    # ... rest of implementation
```

**Runner.put_object (New):**

```python
class Runner:
    def put_object(self, obj: Any) -> ObjectRef:
        """Put object with <app_name>/shared key prefix.
        
        Args:
            obj: Object to cache (will be pickled)
            
        Returns:
            ObjectRef pointing to the cached object
        """
        object_key = ObjectKey.for_shared(self._name)
        return put_object(object_key.to_prefix(), obj)
```

### System Considerations

**Performance:**
- No significant performance impact from additional directory level
- Path construction adds negligible overhead
- Same Arrow IPC format and compression used

**Scalability:**
- Three-level hierarchy allows better organization for many applications
- Shared objects reduce data duplication across sessions
- No change to concurrent operation support

**Reliability:**
- Same atomic write guarantees as current implementation
- Shared objects persist across session lifecycles
- LRU eviction applies equally to shared and session objects

**Resource Usage:**
- Minimal additional memory for longer key strings
- Same disk format, slightly deeper directory structure
- No network overhead changes

**Security:**
- Extended path traversal validation for three components
- No cross-application access control (out of scope)
- Same file permission model

**Observability:**
- Logging includes full three-level keys
- Metrics can differentiate shared vs session objects via key prefix

### Dependencies

**External Dependencies:**
- No new external dependencies

**Internal Dependencies:**
- `object_cache`: Core cache implementation
- `flamepy.runner`: Runner module using new API
- `flamepy.core.cache`: Updated cache client

## 4. Use Cases

### Basic Use Cases

**Example 1: Storing Shared Model Weights**

- Description: A runner stores model weights once for reuse across sessions
- Step-by-step workflow:
  1. Runner application loads model weights
  2. Calls `rr.put_object(weights)` where `rr` is a `Runner("my-model-server")`
  3. Cache stores at key `my-model-server/shared/{uuid}.arrow`
  4. Returns ObjectRef with full key
  5. Future sessions retrieve using same ObjectRef
  6. Model weights available without re-uploading
- Expected outcome: Model weights cached once, accessible to all sessions

**Example 2: Runner Storing Shared Configuration**

- Description: Runner stores shared config for its application
- Step-by-step workflow:
  1. Create Runner for app "my-app"
  2. Call `rr.put_object(shared_config)`
  3. Runner uses "my-app" as app_name automatically
  4. Cache stores at `my-app/shared/{uuid}.arrow`
  5. Other sessions with same app can access via ObjectRef
- Expected outcome: Shared config available to all app sessions

**Example 3: Session-Specific Storage (Internal - RunnerService common_data)**

- Description: RunnerService internally stores RunnerContext as common_data
- Step-by-step workflow:
  1. `RunnerService.__init__` is called with app="my-app"
  2. Generates or uses provided session_id, e.g., "sess-abc"
  3. Serializes RunnerContext and calls `put_object(f"{app}/{session_id}", ctx)`
  4. Cache stores at `my-app/sess-abc/{uuid}.arrow`
  5. ObjectRef is encoded and passed as `common_data` to session
  6. Executors retrieve RunnerContext using the ObjectRef
- Expected outcome: Session-specific RunnerContext stored with app-scoped key

### Advanced Use Cases

**Example 4: Data Pipeline with Shared Intermediate Results**

- Description: Multi-stage pipeline shares intermediate results
- Step-by-step workflow:
  1. Stage 1 session processes raw data
  2. Stores result: `rr.put_object(processed_data)`
  3. Stage 1 session closes
  4. Stage 2 session starts
  5. Retrieves processed_data using ObjectRef from stage 1
  6. Continues processing without re-computation
- Expected outcome: Pipeline stages share data without session coupling

### Test Cases

**Edge Case Tests (for key validation):**

| Test Case | Input | Expected Result |
|-----------|-------|-----------------|
| Empty app name | `/session/uuid` | Error: "app cannot be empty" |
| Empty session | `myapp//uuid` | Error: "session cannot be empty" |
| Empty uuid | `myapp/session/` | Error: "uuid cannot be empty" |
| Path traversal in app | `../etc/session/uuid` | Error: "Invalid key component" |
| Path traversal in session | `myapp/../etc/uuid` | Error: "Invalid key component" |
| Backslash in key | `my\\app/session/uuid` | Error: "Invalid key component" |
| Two-part key (old format) | `session/uuid` | Error: "expected <app>/<ssn>/<uuid>" |
| Four-part key | `a/b/c/d` | Error: "expected <app>/<ssn>/<uuid>" |
| Valid three-part key | `myapp/session/uuid` | Success |
| Reserved session "shared" | `myapp/shared/uuid` | Success (allowed for Runner.put_object) |

**Functional Tests:**

| Test Case | Description | Verification |
|-----------|-------------|--------------|
| Cross-session retrieval | Store via `rr.put_object()`, retrieve from different session | Object content matches |
| Session isolation | Store in `app/session1/`, cannot access with prefix `app/session2/` | Key mismatch error |
| App isolation | Store in `app1/shared/`, cannot access with `app2/shared/` prefix | Key mismatch error |
| Delete session objects | Call `delete_objects("app/session")` | All objects in session removed, other sessions intact |
| Delete app objects | Call `delete_objects("app")` | All objects for app removed |
| LRU eviction with new keys | Fill cache, verify eviction uses correct three-part keys | Evicted keys have correct format |
| Restart recovery | Restart cache server, verify all objects reloaded with correct keys | `load_objects()` reconstructs three-part keys |

**Reserved Name Tests:**

| Test Case | Description | Expected Result |
|-----------|-------------|-----------------|
| Create session with "shared" | `create_session(session_id="shared")` | Error: "Reserved session name" |
| Create session with "default" | `create_session(session_id="default")` | Error: "Reserved session name" |
| Runner.put_object uses shared | `rr.put_object(data)` | Success, key = `<app>/shared/<uuid>` |

## 5. References

### Related Documents
- [RFE318 Object Cache Design](../RFE318-cache/FS.md)
- [RFE423 GitHub Issue](https://github.com/xflops/flame/issues/423)
- [Design Document Template](../templates.md)

### Implementation References
- Cache server: `object_cache/src/cache.rs`
- Disk storage: `object_cache/src/storage/disk.rs`
- Python cache module: `sdk/python/src/flamepy/core/cache.py`
- Python runner module: `sdk/python/src/flamepy/runner/runner.py`

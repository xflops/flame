# RFE429: Support Upload/Download in flame-object-cache

## 1. Motivation

**Background:**

Currently, `flamepy.runner` uses `StorageBackend` implementations (`FileStorage`, `HttpStorage`) to upload application packages. This requires deploying external file servers like dufs for HTTP-based storage.

The issue requests adding `upload_object`/`download_object` to support using `flame-object-cache` as the package storage backend, eliminating external dependencies.

**Target:**

1. Add `upload_object` and `download_object` functions to `flamepy.core.cache` (using existing `do_put`/`do_get`)
2. Add `CacheStorage` backend to `flamepy.runner.storage` that uses these functions
3. Add `grpc://`/`grpcs://` URL scheme support in `executor_manager` for package download
4. Use `grpc://` / `grpcs://` URL scheme for cache-based package storage

## 2. Function Specification

### Key Format

Package keys follow the standard `<app>/<session>/<object_id>` format where:
- `<app>` = application name
- `pkg` = reserved session identifier for packages (like `shared` for shared objects)
- `<filename>` = original package filename (e.g., `myapp-1.0.0.tar.gz`)

**Examples:**
- `myapp/pkg/myapp-1.0.0.tar.gz`
- `model-server/pkg/model-server-0.2.1.tar.gz`

This reuses the existing `ObjectKey` structure without modification.

### Configuration

**Default Behavior (no package config):**

When `package` is not configured, Runner uses flame-object-cache automatically via `cache.endpoint`:

```yaml
# flame.yaml - minimal config, uses cache for packages by default
clusters:
  - name: flame
    endpoint: "http://flame-session-manager:8080"
    cache:
      endpoint: "grpc://flame-object-cache:9090"
```

**Explicit package storage:**

```yaml
# flame.yaml - explicit grpc:// scheme with custom excludes
package:
  storage: "grpc://flame-object-cache:9090"   # or grpcs:// for TLS
  excludes:                    # Optional, merged with defaults
    - "data/"
    - "*.log"

# Or use HTTP backend
package:
  storage: "http://dufs:5000/"
```

**Default excludes** (always applied): `.venv`, `__pycache__`, `.gitignore`, `*.pyc`

**Note:** `file://` storage is not supported for packages - use `http://` or `grpc://` (default).

### API

#### Server-Side: No Changes Required

The existing `do_put` and `do_get` in `flame-object-cache` already support the required functionality:

- **`do_put`**: Accepts `FlightDescriptor.for_path(key)` where key can be 2-part prefix (`<app>/<session>`) or 3-part full key (`<app>/<session>/<object_id>`)
- **`do_get`**: Accepts `Ticket("{key}:{version}")` and returns data stream

The cache server stores objects as Arrow RecordBatch with schema `{version: uint64, data: binary}`. For file upload/download, we store raw file bytes in the `data` field without cloudpickle serialization.

#### Client-Side: New Functions in `flamepy.core.cache`

```python
def upload_object(key_or_prefix: str, file_path: str) -> ObjectRef:
    """Upload a file to the cache using do_put with streaming.
    
    Uses the existing do_put protocol with raw file bytes (no cloudpickle).
    
    Args:
        key_or_prefix: Either full key (e.g., "myapp/pkg/myapp-1.0.0.tar.gz") 
                       or key prefix (e.g., "myapp/pkg"). If prefix, server 
                       generates a UUID for the object_id.
        file_path: Path to the local file to upload
    
    Returns:
        ObjectRef pointing to the uploaded file
        
    Raises:
        ValueError: If cache not configured or upload fails
        FileNotFoundError: If file_path does not exist
    
    Example:
        # With full key (filename preserved)
        ref = upload_object("myapp/pkg/myapp-1.0.0.tar.gz", "/tmp/myapp-1.0.0.tar.gz")
        # ref.key = "myapp/pkg/myapp-1.0.0.tar.gz"
        
        # With prefix (UUID generated)
        ref = upload_object("myapp/pkg", "/tmp/myapp-1.0.0.tar.gz")
        # ref.key = "myapp/pkg/550e8400-e29b-41d4-a716-446655440000"
    """

def download_object(ref: ObjectRef, dest_path: str) -> None:
    """Download a file from the cache using do_get with streaming.
    
    Uses the existing do_get protocol and writes raw bytes to dest_path.
    Supports streaming for large files.
    
    Args:
        ref: ObjectRef pointing to the cached file
        dest_path: Local path to save the downloaded file
        
    Raises:
        ValueError: If object not found or download fails
    
    Example:
        download_object(ref, "/tmp/downloaded.tar.gz")
    """
```

#### New Storage Backend in `flamepy.runner.storage`

```python
class CacheStorage(StorageBackend):
    """Storage backend using flame-object-cache via gRPC."""
    
    def __init__(self, storage_base: Optional[str] = None, app_name: Optional[str] = None):
        """Initialize cache storage backend.
        
        Args:
            storage_base: Optional storage URL (e.g., "grpc://flame-object-cache:9090").
                         If None, uses cache.endpoint from FlameContext.
            app_name: Application name for key prefix. Required for upload.
        """
    
    def upload(self, local_path: str, filename: str) -> str:
        """Upload package to cache, return grpc:// URL."""
    
    def download(self, filename: str, local_path: str) -> None:
        """Download package from cache."""
    
    def delete(self, filename: str) -> None:
        """Delete package from cache."""
```

### URL Scheme

Uses standard gRPC URL format:
- `grpc://<host>:<port>/<key>` - plaintext
- `grpcs://<host>:<port>/<key>` - TLS

Examples:
- `grpc://flame-object-cache:9090/myapp/pkg/myapp-1.0.0.tar.gz`
- `grpcs://10.0.0.1:9090/model-server/pkg/model-server-0.2.1.tar.gz`

### Scope

**In Scope:**
- Client-side: `upload_object()` / `download_object()` in `flamepy.core.cache`
- Client-side: `CacheStorage` backend in `flamepy.runner.storage`
- Executor-side: `grpc://` / `grpcs://` URL scheme support in `executor_manager` package download

**Out of Scope:**
- Server-side changes (existing `do_put`/`do_get` already sufficient)
- Rust common module changes

### Feature Interaction

**Updates Required:**

| File | Changes |
|------|---------|
| `sdk/python/src/flamepy/core/cache.py` | Add `upload_object`, `download_object` with streaming |
| `sdk/python/src/flamepy/core/__init__.py` | Export new functions |
| `sdk/python/src/flamepy/runner/storage.py` | Add `CacheStorage`, update `create_storage_backend()` |
| `executor_manager/src/appmgr/mod.rs` | Add `grpc://`/`grpcs://` support in `download_package()` |

**Integration with Runner:**

```
Runner._start()
    │
    ├─► storage_base = context.package.storage if context.package else None
    │
    ├─► create_storage_backend(storage_base, app_name)
    │       │
    │       ├─► None           → CacheStorage()  ◄── DEFAULT (uses cache.endpoint)
    │       ├─► "grpc://"      → CacheStorage(url)
    │       ├─► "grpcs://"     → CacheStorage(url)
    │       ├─► "http://"      → HttpStorage(url)
    │       └─► other          → ERROR: Unsupported scheme
    │
    ├─► _create_package() → dist/myapp-1.0.0.tar.gz
    │       │
    │       └─► filename includes app name and version (from pyproject.toml/setup.py)
    │
    └─► _upload_package()
            │
            └─► storage_backend.upload(package_path, filename)
                    │   filename = "myapp-1.0.0.tar.gz" (version from project metadata)
                    │
                    └─► Returns grpc://host:port/myapp/pkg/myapp-1.0.0.tar.gz
```

## 3. Implementation Detail

### Architecture

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                          flamepy.runner                                      │
│                                                                              │
│  Runner._start():                                                            │
│    storage_base = context.package.storage if context.package else None      │
│    backend = create_storage_backend(storage_base, app_name)                 │
│    url = backend.upload(package_path, filename)                             │
│    # url = grpc://cache:9090/myapp/pkg/myapp-1.0.0.tar.gz                   │
│    register_application(name, url=url)                                       │
└─────────────────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                      flamepy.runner.storage                                  │
│                                                                              │
│  CacheStorage(StorageBackend):                                              │
│    def upload(local_path, filename) -> str:                                 │
│        # filename already includes version (e.g., "myapp-1.0.0.tar.gz")     │
│        key = f"{self._app_name}/pkg/{filename}"                             │
│        ref = upload_object(key, local_path)                                 │
│        return f"grpc://{self._host}:{self._port}/{ref.key}"                 │
│                                                                              │
│    def download(filename, local_path) -> None:                              │
│        ref = ObjectRef(endpoint=self._endpoint, key=filename, version=0)    │
│        download_object(ref, local_path)                                     │
└─────────────────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                        flamepy.core.cache                                    │
│                                                                              │
│  upload_object(key_or_prefix, file_path) -> ObjectRef:                       │
│    - Stream file in chunks via do_put                                        │
│    - FlightDescriptor.for_path(key_or_prefix)                                │
│    - If prefix (2-part), server generates UUID                               │
│    - Return ObjectRef {key: "myapp/pkg/...", ...}                            │
│                                                                              │
│  download_object(ref, dest_path) -> None:                                   │
│    - do_get with Ticket("{key}:0")                                          │
│    - Stream chunks to dest_path                                             │
└─────────────────────────────────────────────────────────────────────────────┘
                              │
                              ▼ Arrow Flight (gRPC)
┌─────────────────────────────────────────────────────────────────────────────┐
│                    flame-object-cache (NO CHANGES)                           │
│                                                                              │
│  do_put(descriptor, stream)  → existing: stores object, returns metadata    │
│  do_get(ticket)              → existing: returns object data stream         │
│  do_action("DELETE", prefix) → existing: delete objects                     │
└─────────────────────────────────────────────────────────────────────────────┘
```

### Client-Side Implementation

#### `flamepy.core.cache` - upload_object / download_object

```python
# Chunk size for streaming (1MB)
_UPLOAD_CHUNK_SIZE = 1024 * 1024


def upload_object(key_or_prefix: str, file_path: str) -> ObjectRef:
    """Upload a file to the cache using do_put with streaming."""
    import os
    
    if not os.path.exists(file_path):
        raise FileNotFoundError(f"File not found: {file_path}")
    
    # Validate key format (2-part prefix or 3-part full key)
    parts = key_or_prefix.split("/")
    if len(parts) == 2:
        # Prefix: server will generate UUID
        ObjectKey.from_prefix(key_or_prefix)
    elif len(parts) == 3:
        # Full key: use as-is
        ObjectKey.from_key(key_or_prefix)
    else:
        raise ValueError(f"Invalid key format: {key_or_prefix}")
    
    # Get cache config
    context = FlameContext()
    cache_config = context.cache
    
    if cache_config is None:
        raise ValueError("Cache configuration not found")
    
    if isinstance(cache_config, str):
        cache_endpoint = cache_config
        cache_tls = None
    elif isinstance(cache_config, FlameClientCache):
        cache_endpoint = cache_config.endpoint
        cache_tls = cache_config.tls
    else:
        cache_endpoint = cache_config.get("endpoint")
        cache_tls = None
    
    if not cache_endpoint:
        raise ValueError("Cache endpoint not configured")
    
    # Create schema for raw bytes
    schema = pa.schema([
        pa.field("version", pa.uint64()),
        pa.field("data", pa.binary()),
    ])
    
    client = _get_flight_client(cache_endpoint, cache_tls)
    
    # Create descriptor with key_or_prefix (server handles UUID generation if needed)
    descriptor = flight.FlightDescriptor.for_path(key_or_prefix)
    
    # Start streaming upload
    writer, reader = client.do_put(descriptor, schema)
    
    # Stream file in chunks
    file_size = os.path.getsize(file_path)
    with open(file_path, "rb") as f:
        while True:
            chunk = f.read(_UPLOAD_CHUNK_SIZE)
            if not chunk:
                break
            
            batch = pa.RecordBatch.from_arrays(
                [pa.array([0], type=pa.uint64()), pa.array([chunk], type=pa.binary())],
                schema=schema
            )
            writer.write_batch(batch)
    
    writer.done_writing()
    
    # Read result metadata
    try:
        while True:
            metadata_buffer = reader.read()
            if metadata_buffer is None:
                break
            obj_ref_data = bson.decode(bytes(metadata_buffer))
            writer.close()
            ref = ObjectRef(
                endpoint=obj_ref_data["endpoint"],
                key=obj_ref_data["key"],
                version=obj_ref_data["version"],
            )
            logger.debug(f"upload_object: key={ref.key}, version={ref.version}, size={file_size}")
            return ref
    except Exception as e:
        writer.close()
        raise ValueError(f"Failed to read metadata from cache server: {e}")
    
    writer.close()
    raise ValueError("No result metadata received from cache server")


def download_object(ref: ObjectRef, dest_path: str) -> None:
    """Download a file from the cache using do_get with streaming."""
    import os
    
    # Get TLS config
    tls_config = _get_cache_tls_config()
    client = _get_flight_client(ref.endpoint, tls_config)
    
    # Download via do_get (version=0 to force fresh download)
    ticket = flight.Ticket(f"{ref.key}:0".encode())
    reader = client.do_get(ticket)
    
    # Ensure destination directory exists
    dest_dir = os.path.dirname(dest_path)
    if dest_dir and not os.path.exists(dest_dir):
        os.makedirs(dest_dir, exist_ok=True)
    
    # Stream to file
    total_size = 0
    with open(dest_path, "wb") as f:
        for batch in reader:
            # Extract raw bytes from data column
            data_array = batch.column("data")
            for i in range(len(data_array)):
                chunk = data_array[i].as_py()
                if chunk:
                    f.write(chunk)
                    total_size += len(chunk)
    
    if total_size == 0:
        os.remove(dest_path)
        raise ValueError(f"Object not found: {ref.key}")
    
    logger.debug(f"download_object: key={ref.key} -> {dest_path}, size={total_size}")
```

#### `flamepy.runner.storage` - CacheStorage

```python
class CacheStorage(StorageBackend):
    """Storage backend using flame-object-cache via gRPC."""

    def __init__(self, storage_base: Optional[str] = None, app_name: Optional[str] = None):
        """Initialize cache storage backend."""
        from flamepy.core.types import FlameContext, FlameClientCache, FlameError, FlameErrorCode
        
        self._app_name = app_name
        
        if storage_base is not None:
            parsed_url = urlparse(storage_base)
            if parsed_url.scheme not in ("grpc", "grpcs"):
                raise FlameError(FlameErrorCode.INVALID_CONFIG, f"Invalid cache storage URL: {storage_base}")
            self._scheme = parsed_url.scheme
            self._host = parsed_url.hostname or "localhost"
            self._port = parsed_url.port or 9090
        else:
            # Use cache.endpoint from FlameContext
            context = FlameContext()
            if context.cache is None:
                raise FlameError(FlameErrorCode.INVALID_CONFIG, "Cache not configured")
            
            if isinstance(context.cache, FlameClientCache):
                cache_endpoint = context.cache.endpoint
            else:
                cache_endpoint = context.cache if isinstance(context.cache, str) else context.cache.get("endpoint")
            
            if not cache_endpoint:
                raise FlameError(FlameErrorCode.INVALID_CONFIG, "Cache endpoint not configured")
            
            parsed = urlparse(cache_endpoint)
            self._scheme = parsed.scheme
            self._host = parsed.hostname or "localhost"
            self._port = parsed.port or 9090
        
        self._endpoint = f"{self._scheme}://{self._host}:{self._port}"

    def upload(self, local_path: str, filename: str) -> str:
        """Upload a package file to cache storage.
        
        Args:
            local_path: Path to the local package file
            filename: Package filename with version (e.g., "myapp-1.0.0.tar.gz")
        """
        from flamepy.core.cache import upload_object
        
        if not self._app_name:
            raise FlameError(FlameErrorCode.INVALID_CONFIG, "app_name is required for upload")
        
        try:
            # Build full key: <app_name>/pkg/<filename>
            # filename already includes version from Runner (e.g., "myapp-1.0.0.tar.gz")
            key = f"{self._app_name}/pkg/{filename}"
            ref = upload_object(key, local_path)
            url = f"{self._scheme}://{self._host}:{self._port}/{ref.key}"
            logger.debug(f"Uploaded package to cache: {url}")
            return url
        except Exception as e:
            raise FlameError(FlameErrorCode.INTERNAL, f"Failed to upload package to cache: {str(e)}")

    def download(self, filename: str, local_path: str) -> None:
        """Download a package file from cache storage."""
        from flamepy.core.cache import download_object, ObjectRef
        
        try:
            ref = ObjectRef(endpoint=self._endpoint, key=filename, version=0)
            download_object(ref, local_path)
            logger.debug(f"Downloaded package from cache: {filename} -> {local_path}")
        except Exception as e:
            raise FlameError(FlameErrorCode.INTERNAL, f"Failed to download package from cache: {str(e)}")

    def delete(self, filename: str) -> None:
        """Delete a package file from cache storage."""
        from flamepy.core.cache import delete_objects
        
        try:
            delete_objects(filename)
            logger.debug(f"Deleted package from cache: {filename}")
        except Exception as e:
            logger.warning(f"Error deleting package from cache: {e}")


def create_storage_backend(storage_base: Optional[str] = None, app_name: Optional[str] = None) -> StorageBackend:
    """Create a storage backend instance based on the storage URL scheme."""
    if storage_base is None:
        return CacheStorage(app_name=app_name)
    
    parsed_url = urlparse(storage_base)

    if parsed_url.scheme in ("http", "https"):
        return HttpStorage(storage_base)
    elif parsed_url.scheme in ("grpc", "grpcs"):
        return CacheStorage(storage_base, app_name=app_name)
    elif parsed_url.scheme == "file":
        return FileStorage(storage_base)
    else:
        raise FlameError(
            FlameErrorCode.INVALID_CONFIG,
            f"Unsupported storage scheme: {parsed_url.scheme}. Supported: file://, http://, https://, grpc://, grpcs://",
        )
```

### Executor-Side Implementation

#### `executor_manager/src/appmgr/mod.rs` - Package Download with Scheme Registry

Introduce a `PackageDownloader` trait and scheme-based registry for cleaner extensibility:

```rust
use std::collections::HashMap;
use async_trait::async_trait;

/// Trait for package download implementations
#[async_trait]
pub trait PackageDownloader: Send + Sync {
    async fn download(&self, url: &url::Url, dest_path: &PathBuf) -> Result<(), FlameError>;
}

/// File-based package downloader (file://)
pub struct FileDownloader;

#[async_trait]
impl PackageDownloader for FileDownloader {
    async fn download(&self, url: &url::Url, dest_path: &PathBuf) -> Result<(), FlameError> {
        let src_path = url.to_file_path()
            .map_err(|_| FlameError::InvalidConfig(format!("invalid file url: {}", url)))?;
        fs::copy(&src_path, dest_path)
            .map_err(|e| FlameError::Internal(format!("failed to copy: {}", e)))?;
        Ok(())
    }
}

/// HTTP-based package downloader (http://, https://)
pub struct HttpDownloader {
    timeout: Duration,
}

impl HttpDownloader {
    pub fn new(timeout: Duration) -> Self {
        Self { timeout }
    }
}

#[async_trait]
impl PackageDownloader for HttpDownloader {
    async fn download(&self, url: &url::Url, dest_path: &PathBuf) -> Result<(), FlameError> {
        let client = reqwest::Client::builder()
            .timeout(self.timeout)
            .build()
            .map_err(|e| FlameError::Internal(format!("failed to create HTTP client: {}", e)))?;

        let response = client.get(url.as_str()).send().await
            .map_err(|e| FlameError::Internal(format!("failed to download: {}", e)))?;

        if !response.status().is_success() {
            return Err(FlameError::Internal(format!("HTTP {}", response.status())));
        }

        let temp_path = dest_path.with_extension("tmp");
        let mut file = fs::File::create(&temp_path)
            .map_err(|e| FlameError::Internal(format!("failed to create file: {}", e)))?;

        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| FlameError::Internal(format!("stream error: {}", e)))?;
            file.write_all(&chunk)
                .map_err(|e| FlameError::Internal(format!("write error: {}", e)))?;
        }

        file.sync_all().map_err(|e| FlameError::Internal(format!("sync error: {}", e)))?;
        drop(file);
        fs::rename(&temp_path, dest_path)
            .map_err(|e| FlameError::Internal(format!("rename error: {}", e)))?;

        Ok(())
    }
}

/// gRPC/Arrow Flight-based package downloader (grpc://, grpcs://)
pub struct GrpcDownloader;

#[async_trait]
impl PackageDownloader for GrpcDownloader {
    async fn download(&self, url: &url::Url, dest_path: &PathBuf) -> Result<(), FlameError> {
        use arrow_flight::{FlightClient, Ticket};
        use arrow::array::BinaryArray;
        use tonic::transport::Channel;

        let host = url.host_str()
            .ok_or_else(|| FlameError::InvalidConfig("missing host".to_string()))?;
        let port = url.port().unwrap_or(9090);

        let endpoint = if url.scheme() == "grpcs" {
            format!("https://{}:{}", host, port)
        } else {
            format!("http://{}:{}", host, port)
        };

        let key = url.path().trim_start_matches('/');

        let channel = Channel::from_shared(endpoint)
            .map_err(|e| FlameError::Internal(format!("invalid endpoint: {}", e)))?
            .connect()
            .await
            .map_err(|e| FlameError::Internal(format!("connect failed: {}", e)))?;

        let mut client = FlightClient::new(channel);
        let ticket = Ticket::new(format!("{}:0", key));
        let mut stream = client.do_get(ticket).await
            .map_err(|e| FlameError::Internal(format!("do_get failed: {}", e)))?;

        let temp_path = dest_path.with_extension("tmp");
        let mut file = fs::File::create(&temp_path)
            .map_err(|e| FlameError::Internal(format!("failed to create file: {}", e)))?;

        let mut total_size = 0usize;
        while let Some(batch) = stream.try_next().await
            .map_err(|e| FlameError::Internal(format!("stream error: {}", e)))?
        {
            if let Some(array) = batch.column_by_name("data") {
                if let Some(binary_array) = array.as_any().downcast_ref::<BinaryArray>() {
                    for i in 0..binary_array.len() {
                        let chunk = binary_array.value(i);
                        file.write_all(chunk)
                            .map_err(|e| FlameError::Internal(format!("write error: {}", e)))?;
                        total_size += chunk.len();
                    }
                }
            }
        }

        if total_size == 0 {
            fs::remove_file(&temp_path).ok();
            return Err(FlameError::Internal(format!("object not found: {}", key)));
        }

        file.sync_all().map_err(|e| FlameError::Internal(format!("sync error: {}", e)))?;
        drop(file);
        fs::rename(&temp_path, dest_path)
            .map_err(|e| FlameError::Internal(format!("rename error: {}", e)))?;

        tracing::info!("Downloaded package via gRPC: {} ({} bytes)", key, total_size);
        Ok(())
    }
}

/// Registry mapping URL schemes to downloaders
pub struct DownloaderRegistry {
    downloaders: HashMap<String, Box<dyn PackageDownloader>>,
}

impl DownloaderRegistry {
    pub fn new() -> Self {
        let mut downloaders: HashMap<String, Box<dyn PackageDownloader>> = HashMap::new();
        
        // Register default downloaders
        downloaders.insert("file".to_string(), Box::new(FileDownloader));
        downloaders.insert("http".to_string(), Box::new(HttpDownloader::new(Duration::from_secs(300))));
        downloaders.insert("https".to_string(), Box::new(HttpDownloader::new(Duration::from_secs(300))));
        downloaders.insert("grpc".to_string(), Box::new(GrpcDownloader));
        downloaders.insert("grpcs".to_string(), Box::new(GrpcDownloader));
        
        Self { downloaders }
    }

    pub async fn download(&self, url: &str, dest_path: &PathBuf) -> Result<(), FlameError> {
        let parsed_url = url::Url::parse(url)
            .map_err(|e| FlameError::InvalidConfig(format!("invalid url: {}", e)))?;

        let scheme = parsed_url.scheme();
        let downloader = self.downloaders.get(scheme)
            .ok_or_else(|| FlameError::InvalidConfig(format!(
                "unsupported scheme: {}. Supported: file, http, https, grpc, grpcs", scheme
            )))?;

        downloader.download(&parsed_url, dest_path).await
    }
}

impl Default for DownloaderRegistry {
    fn default() -> Self {
        Self::new()
    }
}
```

**Usage in `ApplicationManager`:**

```rust
impl ApplicationManager {
    pub fn new() -> Result<Self, FlameError> {
        Ok(Self {
            apps: Arc::new(std::sync::Mutex::new(HashMap::new())),
            flame_home: env::var("FLAME_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("/opt/flame")),
            downloader: DownloaderRegistry::new(),
        })
    }

    async fn download_package(&self, url: &str, app_name: &str) -> Result<PathBuf, FlameError> {
        let download_dir = self.flame_home
            .join("data/apps")
            .join(app_name)
            .join("download");
        fs::create_dir_all(&download_dir)?;

        let parsed_url = url::Url::parse(url)?;
        let filename = parsed_url.path_segments()
            .and_then(|mut s| s.next_back())
            .unwrap_or("package.tar.gz");

        let package_path = download_dir.join(filename);

        if package_path.exists() {
            tracing::debug!("Package already downloaded: {}", package_path.display());
            return Ok(package_path);
        }

        self.downloader.download(url, &package_path).await?;
        
        tracing::info!("Downloaded package to: {}", package_path.display());
        Ok(package_path)
    }
}
```
```

## 4. Use Cases

### Use Case 1: Runner with Default Config (No package section)

**Configuration:**
```yaml
# flame.yaml - minimal config
clusters:
  - name: flame
    endpoint: "http://flame-session-manager:8080"
    cache:
      endpoint: "grpc://flame-object-cache:9090"
```

**Usage:**
```python
from flamepy.runner import Runner

with Runner("my-ml-app") as runner:
    svc = runner.service(MyModel)
    # Package created: dist/my-ml-app-1.0.0.tar.gz
    # Uploaded to: grpc://flame-object-cache:9090/my-ml-app/pkg/my-ml-app-1.0.0.tar.gz
    
    results = [svc.predict(x) for x in data]
```

### Use Case 2: Direct Upload/Download API

```python
from flamepy.core.cache import upload_object, download_object

# Upload with full key (filename preserved)
ref = upload_object("training/pkg/model-v2.tar.gz", "/data/model-v2.tar.gz")
print(f"Uploaded to: {ref.key}")  # "training/pkg/model-v2.tar.gz"

# Upload with prefix (UUID generated by server)
ref = upload_object("training/pkg", "/data/model-v2.tar.gz")
print(f"Uploaded to: {ref.key}")  # "training/pkg/550e8400-e29b-41d4-..."

# Download on another machine
download_object(ref, "/tmp/model-v2.tar.gz")
```

### Use Case 3: Explicit HTTP Storage (Legacy)

```yaml
# flame.yaml - explicit HTTP storage (e.g., existing dufs setup)
package:
  storage: "http://dufs:5000/"
```

```python
# Works unchanged - uses HttpStorage backend
with Runner("my-ml-app") as runner:
    svc = runner.service(MyModel)
```

## 5. References

### Related Documents
- [RFE429 GitHub Issue](https://github.com/xflops/flame/issues/429)
- [RFE318 Cache Design](../RFE318-cache/FS.md)

### Implementation References
- Cache server: `object_cache/src/cache.rs`
- Cache module: `sdk/python/src/flamepy/core/cache.py`
- Storage backends: `sdk/python/src/flamepy/runner/storage.py`
- Runner: `sdk/python/src/flamepy/runner/runner.py`
- Executor app manager: `executor_manager/src/appmgr/mod.rs`

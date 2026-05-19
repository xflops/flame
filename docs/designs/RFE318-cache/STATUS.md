# Object Cache Implementation Status

## ✅ Implemented: Native DataSet/DataFrame Direct Cache Path

Issue #318 item 3 is implemented for tabular payloads that can be represented as Arrow data directly in object cache, instead of wrapping them as pickled objects or Arrow IPC bytes inside the opaque `{version, data}` row.

Scope:
- Detect PyArrow tables/batches, pandas DataFrames, polars DataFrames, and adapter-backed Dataset-like objects in FlamePy without adding mandatory pandas/polars/datasets dependencies.
- Stream and persist native tabular payloads as Arrow schemas and record batches with reserved `flame.cache.*` schema metadata.
- Preserve the existing opaque object path, existing `ObjectRef` shape, and legacy `version/data` cache files.
- Keep native tabular patch/merge semantics out of scope for this item.

Verification:
- Rust object-cache unit coverage includes native Arrow payload storage, reload, and Flight `do_get` schema/batch streaming.
- Python unit coverage includes native payload classification, direct remote `do_put` schema emission, and native Arrow `do_get` parsing.
- E2E tests were added for native Arrow table put/get and update paths.

## Migration from Naive Cache

✅ **Completed**: The naive HTTP-based cache in `flame-executor-manager` has been removed. The following changes were made:
- Removed `executor_manager/src/cache/mod.rs` and `executor_manager/src/cache/types.rs`
- Removed cache thread startup code from `executor_manager/src/main.rs`
- Removed unnecessary dependencies: `actix-web`, `bson`, `network-interface`, `regex`
- Updated documentation to reflect the migration

The object cache is now provided as an embedded library within the `flame-executor-manager` service, running in a dedicated thread.

## ✅ Completed

### Core Implementation
- ✅ Rust Arrow Flight server implemented (`object_cache/src/cache.rs`)
- ✅ Disk persistence using Arrow IPC format
- ✅ Key-based storage organization (`app_name/session_id/object_id`)
- ✅ In-memory index with HashMap
- ✅ Object loading from disk on startup
- ✅ Configuration support (flame-cluster.yaml with storage path)
- ✅ Docker integration (embedded in executor-manager, compose.yaml, Makefile)

### API Operations
- ✅ `get_flight_info`: Returns flight metadata for objects
- ✅ `do_get`: Retrieves objects by key (ticket)
- ✅ `list_flights`: Lists all cached objects
- ✅ `do_put`: Uploads objects (schema decoding fixed)
- ✅ `get_schema`: Returns object schema
- ✅ Schema encoding/decoding compatibility between Rust and Python resolved

### Python SDK
- ✅ Updated `ObjectRef` structure (endpoint, key, version)
- ✅ Arrow Flight client implementation
- ✅ Added pyarrow dependency
- ✅ Updated `FlameContext` to support cache configuration
- ✅ Schema encoding using Arrow IPC messages

### Build & Deployment
- ✅ Successfully builds with arrow 53 and tonic 0.12
- ✅ Docker images build successfully
- ✅ Services start and run properly

## 🔧 Remaining Issues

### Python SDK - `do_put` Metadata Reading
**Issue**: Python client cannot read the `PutResult` metadata from the Rust server's response.

**Error**: `AttributeError: 'NoneType' object has no attribute 'app_metadata'`

**Root Cause**: The Python pyarrow Flight client's `reader.read()` returns `None` after `do_put`, suggesting:
1. The Rust server's `PutResult` stream might not be properly consumed by Python
2. The metadata format might not match what pyarrow expects
3. The streaming pattern might need adjustment

**Attempted Solutions**:
1. ✅ Fixed schema encoding/decoding (now works)
2. ✅ Changed session_id from app_metadata to FlightDescriptor path
3. ❌ Various approaches to read metadata from reader (all return None)

**Next Steps**:
1. Research pyarrow Flight `do_put` metadata handling
2. Consider alternative approaches:
   - Return metadata in response headers instead of PutResult stream
   - Use `do_action` for put operations
   - Modify the streaming pattern to match pyarrow expectations

## 📊 Test Results

**Current Status**: 28 failed, 23 passed, 1 skipped

**Passing Tests**: Core application tests, agent tests without cache
**Failing Tests**: All tests using cache (put_object, get_object, update_object)

## 🔍 Technical Details

### Schema Encoding Fix
Changed from `StreamWriter` (which creates full IPC stream) to `IpcDataGenerator.schema_to_bytes()` (which creates just the schema message).

### Schema Decoding Fix  
Changed from `StreamReader` to `root_as_message` + `fb_to_schema` to properly decode IPC schema messages from Python.

### Session ID Transmission
Moved from HTTP headers/app_metadata to `FlightDescriptor.path[0]` for better compatibility.

## 📝 Files Modified

**Rust**:
- `object_cache/src/cache.rs` - Core implementation
- `object_cache/Cargo.toml` - Dependencies (arrow 53)
- `common/src/ctx.rs` - Added storage field

**Python**:
- `sdk/python/src/flamepy/core/cache.py` - Arrow Flight client
- `sdk/python/src/flamepy/core/types.py` - Updated FlameContext
- `sdk/python/pyproject.toml` - Added pyarrow dependency

**Configuration**:
- `ci/flame-cluster.yaml` - Updated cache config
- `ci/flame.yaml` - Updated client cache config
- `compose.yaml` - Added cache service
- `Makefile` - Added cache build targets

**Docker**:
- `object_cache/src/lib.rs` - Cache library entry point

**Documentation**:
- `object_cache/README.md` - Usage documentation
- `object_cache/IMPLEMENTATION.md` - Implementation details

## 🚀 Quick Test

To test manually:
```bash
# Start services
docker compose up -d

# Check cache logs
docker compose logs flame-executor-manager | grep cache

# Run specific cache test
docker compose exec flame-console uv run pytest -vv tests/test_cache.py::test_objectref_encode_decode
```

## 💡 Recommendations

1. **Immediate**: Fix the `do_put` metadata reading issue
   - Option A: Research pyarrow examples with PutResult metadata
   - Option B: Use HTTP response headers for metadata
   - Option C: Implement `do_action` for put/get operations

2. **Testing**: Once metadata reading works, verify:
   - Object persistence across server restarts
   - Concurrent cache operations
   - Large object handling

3. **Performance**: Benchmark and optimize:
   - IPC encoding/decoding overhead
   - Disk I/O patterns
   - Memory usage with large caches

4. **Documentation**: Update:
   - API usage examples
   - Configuration options
   - Troubleshooting guide

## 📚 References

- Design Document: `docs/designs/RFE318-cache/FS.md`
- Arrow Flight Spec: https://arrow.apache.org/docs/format/Flight.html
- PyArrow Flight: https://arrow.apache.org/docs/python/api/flight.html
- Arrow IPC: https://arrow.apache.org/docs/format/Columnar.html#ipc-file-format

## ✅ Completed (Patch Operation)

### Core Implementation
- ✅ `patch` operation implemented in `ObjectCache` (append-only semantics)
- ✅ `PATCH` action added to `FlightCacheServer`
- ✅ `deltas` field added to `Object` struct
- ✅ `get_object` updated to return base object + deltas

### Python SDK
- ✅ `patch_object` function added to `flamepy.core.cache`
- ✅ `get_object` updated to handle `{base, deltas}` response structure

### Testing
- ✅ Comprehensive unit tests for `patch` operation added to `e2e/tests/test_cache.py`
